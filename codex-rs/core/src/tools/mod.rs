pub mod context;
pub(crate) mod handlers;
pub mod registry;
pub mod router;
pub mod spec;

use crate::apply_patch;
use crate::apply_patch::ApplyPatchExec;
use crate::apply_patch::InternalApplyPatchInvocation;
use crate::apply_patch::convert_apply_patch_to_protocol;
use crate::codex::Session;
use crate::codex::TurnContext;
use crate::error::CodexErr;
use crate::error::SandboxErr;
use crate::exec::ExecParams;
use crate::exec::ExecToolCallOutput;
use crate::exec::StdoutStream;
use crate::executor::ExecutionMode;
use crate::executor::errors::ExecError;
use crate::executor::linkers::PreparedExec;
use crate::function_tool::FunctionCallError;
use crate::tools::context::ApplyPatchCommandContext;
use crate::tools::context::ExecCommandContext;
use crate::turn_diff_tracker::TurnDiffTracker;
use codex_apply_patch::MaybeApplyPatchVerified;
use codex_apply_patch::maybe_parse_apply_patch_verified;
use codex_protocol::protocol::AskForApproval;
use codex_utils_string::take_bytes_at_char_boundary;
use codex_utils_string::take_last_bytes_at_char_boundary;
pub use router::ToolRouter;
use serde::Serialize;
use tracing::trace;

// Model-formatting limits: clients get full streams; only content sent to the model is truncated.
pub(crate) const MODEL_FORMAT_MAX_BYTES: usize = 10 * 1024; // 10 KiB
pub(crate) const MODEL_FORMAT_MAX_LINES: usize = 256; // lines
pub(crate) const MODEL_FORMAT_HEAD_LINES: usize = MODEL_FORMAT_MAX_LINES / 2;
pub(crate) const MODEL_FORMAT_TAIL_LINES: usize = MODEL_FORMAT_MAX_LINES - MODEL_FORMAT_HEAD_LINES; // 128
pub(crate) const MODEL_FORMAT_HEAD_BYTES: usize = MODEL_FORMAT_MAX_BYTES / 2;

// Telemetry preview limits: keep log events smaller than model budgets.
pub(crate) const TELEMETRY_PREVIEW_MAX_BYTES: usize = 2 * 1024; // 2 KiB
pub(crate) const TELEMETRY_PREVIEW_MAX_LINES: usize = 64; // lines
pub(crate) const TELEMETRY_PREVIEW_TRUNCATION_NOTICE: &str =
    "[... telemetry preview truncated ...]";

// TODO(jif) break this down
pub(crate) struct HandleExecRequest<'a> {
    pub tool_name: &'a str,
    pub params: ExecParams,
    pub sess: &'a Session,
    pub turn_context: &'a TurnContext,
    pub turn_diff_tracker: &'a mut TurnDiffTracker,
    pub sub_id: String,
    pub call_id: String,
}

pub(crate) async fn handle_container_exec_with_params(
    request: HandleExecRequest<'_>,
) -> Result<String, FunctionCallError> {
    let HandleExecRequest {
        tool_name,
        params,
        sess,
        turn_context,
        turn_diff_tracker,
        sub_id,
        call_id,
    } = request;
    let otel_event_manager = turn_context.client.get_otel_event_manager();

    if params.with_escalated_permissions.unwrap_or(false)
        && !matches!(turn_context.approval_policy, AskForApproval::OnRequest)
    {
        return Err(FunctionCallError::RespondToModel(format!(
            "approval policy is {policy:?}; reject command — you should not ask for escalated permissions if the approval policy is {policy:?}",
            policy = turn_context.approval_policy
        )));
    }

    // check if this was a patch, and apply it if so
    let apply_patch_exec = match maybe_parse_apply_patch_verified(&params.command, &params.cwd) {
        MaybeApplyPatchVerified::Body(changes) => {
            match apply_patch::apply_patch(sess, turn_context, &sub_id, &call_id, changes).await {
                InternalApplyPatchInvocation::Output(item) => return item,
                InternalApplyPatchInvocation::DelegateToExec(apply_patch_exec) => {
                    Some(apply_patch_exec)
                }
            }
        }
        MaybeApplyPatchVerified::CorrectnessError(parse_error) => {
            // It looks like an invocation of `apply_patch`, but we
            // could not resolve it into a patch that would apply
            // cleanly. Return to model for resample.
            return Err(FunctionCallError::RespondToModel(format!(
                "apply_patch verification failed: {parse_error}"
            )));
        }
        MaybeApplyPatchVerified::ShellParseError(error) => {
            trace!("Failed to parse shell command, {error:?}");
            None
        }
        MaybeApplyPatchVerified::NotApplyPatch => None,
    };

    let command_for_display = if let Some(exec) = apply_patch_exec.as_ref() {
        vec!["apply_patch".to_string(), exec.action.patch.clone()]
    } else {
        params.command.clone()
    };

    let exec_command_context = ExecCommandContext {
        sub_id: sub_id.clone(),
        call_id: call_id.clone(),
        command_for_display: command_for_display.clone(),
        cwd: params.cwd.clone(),
        apply_patch: apply_patch_exec.as_ref().map(
            |ApplyPatchExec {
                 action,
                 user_explicitly_approved_this_action,
             }| ApplyPatchCommandContext {
                user_explicitly_approved_this_action: *user_explicitly_approved_this_action,
                changes: convert_apply_patch_to_protocol(action),
            },
        ),
        tool_name: tool_name.to_string(),
        otel_event_manager,
    };

    let mode = match apply_patch_exec {
        Some(exec) => ExecutionMode::ApplyPatch(exec),
        None => ExecutionMode::Shell,
    };

    sess.services.executor.update_environment(
        turn_context.sandbox_policy.clone(),
        turn_context.cwd.clone(),
    );

    let prepared_exec = PreparedExec::new(
        exec_command_context,
        params,
        command_for_display,
        mode,
        Some(StdoutStream {
            sub_id: sub_id.clone(),
            call_id: call_id.clone(),
            tx_event: sess.get_tx_event(),
        }),
        turn_context.shell_environment_policy.use_profile,
    );

    let output_result = sess
        .run_exec_with_events(
            turn_diff_tracker,
            prepared_exec,
            turn_context.approval_policy,
        )
        .await;

    match output_result {
        Ok(output) => {
            let ExecToolCallOutput { exit_code, .. } = &output;
            let content = format_exec_output_apply_patch(&output);
            if *exit_code == 0 {
                Ok(content)
            } else {
                Err(FunctionCallError::RespondToModel(content))
            }
        }
        Err(ExecError::Function(err)) => Err(err),
        Err(ExecError::Codex(CodexErr::Sandbox(SandboxErr::Timeout { output }))) => Err(
            FunctionCallError::RespondToModel(format_exec_output_apply_patch(&output)),
        ),
        Err(ExecError::Codex(err)) => Err(FunctionCallError::RespondToModel(
            format_unexpected_exec_error(err),
        )),
    }
}

pub fn format_exec_output_apply_patch(exec_output: &ExecToolCallOutput) -> String {
    let ExecToolCallOutput {
        exit_code,
        duration,
        ..
    } = exec_output;

    #[derive(Serialize)]
    struct ExecMetadata {
        exit_code: i32,
        duration_seconds: f32,
    }

    #[derive(Serialize)]
    struct ExecOutput<'a> {
        output: &'a str,
        structured_output: &'a str,
        metadata: ExecMetadata,
    }

    // round to 1 decimal place
    let duration_seconds = ((duration.as_secs_f32()) * 10.0).round() / 10.0;

    let formatted_output = format_exec_output_str(exec_output);
    let structured_output = format_exec_output_structured(exec_output);

    let payload = ExecOutput {
        output: &formatted_output,
        structured_output: &structured_output,
        metadata: ExecMetadata {
            exit_code: *exit_code,
            duration_seconds,
        },
    };

    #[expect(clippy::expect_used)]
    serde_json::to_string(&payload).expect("serialize ExecOutput")
}

fn format_unexpected_exec_error(err: CodexErr) -> String {
    format!("execution error: {err:?}")
}

#[cfg(test)]
fn format_structured_error(message: &str) -> String {
    let lines = [
        "Exit code: N/A".to_string(),
        "Wall time: N/A seconds".to_string(),
        format!("Error: {message}"),
        "Output:".to_string(),
        String::new(),
    ];
    lines.join("\n")
}

fn format_wall_time(duration: std::time::Duration) -> String {
    format_significant_digits(duration.as_secs_f64(), 4)
}

fn format_significant_digits(value: f64, digits: usize) -> String {
    if !value.is_finite() {
        return value.to_string();
    }
    if value == 0.0 {
        return "0".to_string();
    }

    let abs = value.abs();
    let initial_exponent = abs.log10().floor() as i32;
    let rounded_value = if value == 0.0 {
        0.0
    } else {
        let scale = 10_f64.powf((digits as f64 - 1.0) - initial_exponent as f64);
        (value * scale).round() / scale
    };

    let abs_rounded = rounded_value.abs();
    let exponent = if abs_rounded == 0.0 {
        0
    } else {
        abs_rounded.log10().floor() as i32
    };
    let use_exp = exponent < -4 || exponent >= digits as i32;
    if use_exp {
        return format!("{rounded_value:.prec$e}", prec = digits.saturating_sub(1));
    }

    let decimal_places = (digits as i32 - exponent - 1).max(0) as usize;
    let mut s = format!("{rounded_value:.decimal_places$}");
    if s.contains('.') {
        while s.ends_with('0') {
            s.pop();
        }
        if s.ends_with('.') {
            s.pop();
        }
    }
    s
}

pub fn format_exec_output_structured(exec_output: &ExecToolCallOutput) -> String {
    let ExecToolCallOutput {
        exit_code,
        duration,
        aggregated_output,
        ..
    } = exec_output;

    let mut sections = Vec::new();
    sections.push(format!("Exit code: {exit_code}"));
    sections.push(format!(
        "Wall time: {} seconds",
        format_wall_time(*duration)
    ));

    if let Some(total_lines) = aggregated_output.truncated_after_lines {
        sections.push(format!("Total output lines: {total_lines}"));
    }

    sections.push("Output:".to_string());
    sections.push(format_exec_output_str(exec_output));

    sections.join("\n")
}

pub fn format_exec_output_str(exec_output: &ExecToolCallOutput) -> String {
    let ExecToolCallOutput {
        aggregated_output, ..
    } = exec_output;

    // Head+tail truncation for the model: show the beginning and end with an elision.
    // Clients still receive full streams; only this formatted summary is capped.

    let mut s = &aggregated_output.text;
    let prefixed_str: String;

    if exec_output.timed_out {
        prefixed_str = format!(
            "command timed out after {} milliseconds\n",
            exec_output.duration.as_millis()
        ) + s;
        s = &prefixed_str;
    }

    let total_lines = s.lines().count();
    if s.len() <= MODEL_FORMAT_MAX_BYTES && total_lines <= MODEL_FORMAT_MAX_LINES {
        return s.to_string();
    }

    let segments: Vec<&str> = s.split_inclusive('\n').collect();
    let head_take = MODEL_FORMAT_HEAD_LINES.min(segments.len());
    let tail_take = MODEL_FORMAT_TAIL_LINES.min(segments.len().saturating_sub(head_take));
    let omitted = segments.len().saturating_sub(head_take + tail_take);

    let head_slice_end: usize = segments
        .iter()
        .take(head_take)
        .map(|segment| segment.len())
        .sum();
    let tail_slice_start: usize = if tail_take == 0 {
        s.len()
    } else {
        s.len()
            - segments
                .iter()
                .rev()
                .take(tail_take)
                .map(|segment| segment.len())
                .sum::<usize>()
    };
    let marker = format!("\n[... omitted {omitted} of {total_lines} lines ...]\n\n");

    // Byte budgets for head/tail around the marker
    let mut head_budget = MODEL_FORMAT_HEAD_BYTES.min(MODEL_FORMAT_MAX_BYTES);
    let tail_budget = MODEL_FORMAT_MAX_BYTES.saturating_sub(head_budget + marker.len());
    if tail_budget == 0 && marker.len() >= MODEL_FORMAT_MAX_BYTES {
        // Degenerate case: marker alone exceeds budget; return a clipped marker
        return take_bytes_at_char_boundary(&marker, MODEL_FORMAT_MAX_BYTES).to_string();
    }
    if tail_budget == 0 {
        // Make room for the marker by shrinking head
        head_budget = MODEL_FORMAT_MAX_BYTES.saturating_sub(marker.len());
    }

    let head_slice = &s[..head_slice_end];
    let head_part = take_bytes_at_char_boundary(head_slice, head_budget);
    let mut result = String::with_capacity(MODEL_FORMAT_MAX_BYTES.min(s.len()));

    result.push_str(head_part);
    result.push_str(&marker);

    let remaining = MODEL_FORMAT_MAX_BYTES.saturating_sub(result.len());
    if remaining == 0 {
        return result;
    }

    let tail_slice = &s[tail_slice_start..];
    let tail_part = take_last_bytes_at_char_boundary(tail_slice, remaining);
    result.push_str(tail_part);

    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::StreamOutput;
    use pretty_assertions::assert_eq;
    use serde::Deserialize;
    use std::time::Duration;
    const TRUNCATED_STRUCTURED_EXPECTED: &str =
        include_str!("tests/truncated_structured_expected.txt");

    fn sample_output() -> ExecToolCallOutput {
        ExecToolCallOutput {
            exit_code: 0,
            stdout: StreamOutput::new("stdout".to_string()),
            stderr: StreamOutput::new("stderr".to_string()),
            aggregated_output: StreamOutput::new("stdout\nstderr".to_string()),
            duration: Duration::from_secs_f64(1.2345),
            timed_out: false,
        }
    }

    #[test]
    fn structured_format_basic() {
        let formatted = format_exec_output_structured(&sample_output());
        let expected = "Exit code: 0\nWall time: 1.235 seconds\nOutput:\nstdout\nstderr";
        assert_eq!(formatted, expected);
    }

    #[test]
    fn structured_format_includes_truncation_metadata() {
        let mut output = sample_output();
        output.aggregated_output.truncated_after_lines = Some(200);
        let formatted = format_exec_output_structured(&output);
        assert!(formatted.contains("Total output lines: 200"));
    }

    #[test]
    fn significant_digit_formatting_matches_expectations() {
        assert_eq!(format_significant_digits(0.0, 4), "0");
        assert_eq!(format_significant_digits(1.23456, 4), "1.235");
        assert_eq!(format_significant_digits(12345.0, 4), "1.235e4");
        assert_eq!(format_significant_digits(0.000123456, 4), "0.0001235");
    }

    #[test]
    fn structured_error_includes_metadata() {
        let error = format_structured_error("unexpected failure");
        assert_eq!(
            error,
            "Exit code: N/A\nWall time: N/A seconds\nError: unexpected failure\nOutput:\n"
        );
    }

    #[test]
    fn format_exec_output_apply_patch_produces_expected_payload() {
        #[derive(Deserialize)]
        struct ExecMetadata {
            exit_code: i32,
            duration_seconds: f32,
        }

        #[derive(Deserialize)]
        struct ExecOutput {
            output: String,
            structured_output: String,
            metadata: ExecMetadata,
        }

        let output = sample_output();
        let formatted = format_exec_output_apply_patch(&output);
        let payload: ExecOutput =
            serde_json::from_str(&formatted).expect("exec output should be valid json");

        assert_eq!(payload.output, "stdout\nstderr");
        assert_eq!(
            payload.structured_output,
            "Exit code: 0\nWall time: 1.235 seconds\nOutput:\nstdout\nstderr"
        );
        assert_eq!(payload.metadata.exit_code, 0);
        assert_eq!(payload.metadata.duration_seconds, 1.2);
    }

    #[test]
    fn format_exec_output_truncates_long_output() {
        #[derive(Deserialize)]
        struct ExecOutput {
            structured_output: String,
        }

        let mut output = sample_output();
        let mut aggregated = String::new();
        for i in 0..260 {
            aggregated.push_str(&format!("L{i:03}\n"));
        }
        output.aggregated_output = StreamOutput::new(aggregated);

        let formatted = format_exec_output_apply_patch(&output);
        let parsed: ExecOutput =
            serde_json::from_str(&formatted).expect("exec output should be valid json");
        assert_eq!(parsed.structured_output, TRUNCATED_STRUCTURED_EXPECTED);
    }
}
