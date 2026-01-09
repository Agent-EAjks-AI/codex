use std::cell::RefCell;
use std::collections::HashMap;

use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use crossterm::event::KeyModifiers;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Stylize;
use ratatui::text::Line;
use ratatui::text::Span;
use ratatui::widgets::Clear;
use ratatui::widgets::Paragraph;
use ratatui::widgets::StatefulWidgetRef;
use ratatui::widgets::Widget;

use codex_core::protocol::Op;
use codex_core::protocol::SkillDependency;

use crate::app_event::AppEvent;
use crate::app_event_sender::AppEventSender;
use crate::bottom_pane::BottomPaneView;
use crate::bottom_pane::CancellationEvent;
use crate::bottom_pane::textarea::TextArea;
use crate::bottom_pane::textarea::TextAreaState;
use crate::key_hint;
use crate::render::renderable::Renderable;

pub(crate) struct DependencyInputView {
    request_id: String,
    skill_name: String,
    dependencies: Vec<SkillDependency>,
    app_event_tx: AppEventSender,
    textarea: TextArea,
    textarea_state: RefCell<TextAreaState>,
    complete: bool,
    current_index: usize,
    values: HashMap<String, String>,
}

impl DependencyInputView {
    pub(crate) fn new(
        request_id: String,
        skill_name: String,
        dependencies: Vec<SkillDependency>,
        app_event_tx: AppEventSender,
    ) -> Self {
        Self {
            request_id,
            skill_name,
            dependencies,
            app_event_tx,
            textarea: TextArea::new(),
            textarea_state: RefCell::new(TextAreaState::default()),
            complete: false,
            current_index: 0,
            values: HashMap::new(),
        }
    }

    fn submit(&mut self, values: HashMap<String, String>) {
        self.app_event_tx
            .send(AppEvent::CodexOp(Op::ResolveSkillDependencies {
                id: self.request_id.clone(),
                values,
            }));
        self.complete = true;
    }

    fn advance_with_value(&mut self) {
        let Some(dependency) = self.dependencies.get(self.current_index) else {
            return;
        };
        let value = self.textarea.text().trim().to_string();
        if value.is_empty() {
            return;
        }
        self.values.insert(dependency.name.clone(), value);
        self.textarea.set_text_clearing_elements("");
        if self.current_index + 1 >= self.dependencies.len() {
            let values = std::mem::take(&mut self.values);
            self.submit(values);
        } else {
            self.current_index = self.current_index.saturating_add(1);
        }
    }
}

impl BottomPaneView for DependencyInputView {
    fn handle_key_event(&mut self, key_event: KeyEvent) {
        match key_event {
            KeyEvent {
                code: KeyCode::Esc, ..
            } => {
                self.on_ctrl_c();
            }
            KeyEvent {
                code: KeyCode::Enter,
                modifiers,
                ..
            } if modifiers.contains(KeyModifiers::CONTROL) => {
                self.advance_with_value();
            }
            KeyEvent {
                code: KeyCode::Enter,
                ..
            } => self.advance_with_value(),
            other => {
                self.textarea.input(other);
            }
        }
    }

    fn on_ctrl_c(&mut self) -> CancellationEvent {
        self.submit(HashMap::new());
        CancellationEvent::Handled
    }

    fn is_complete(&self) -> bool {
        self.complete
    }

    fn handle_paste(&mut self, pasted: String) -> bool {
        if pasted.is_empty() {
            return false;
        }
        self.textarea.insert_str(&pasted);
        true
    }
}

impl Renderable for DependencyInputView {
    fn desired_height(&self, width: u16) -> u16 {
        let input_height = self.input_height(width);
        5u16 + input_height + 3u16
    }

    fn render(&self, area: Rect, buf: &mut Buffer) {
        if area.height == 0 || area.width == 0 {
            return;
        }

        let input_height = self.input_height(area.width);
        let mut cursor_y = area.y;

        let hint_area = Rect {
            x: area.x,
            y: cursor_y,
            width: area.width,
            height: 1,
        };
        let hint_spans: Vec<Span<'static>> = vec![
            gutter(),
            format!(
                "${name} relies on one or more environment variables.",
                name = self.skill_name
            )
            .into(),
        ];
        Paragraph::new(Line::from(hint_spans)).render(hint_area, buf);
        cursor_y = cursor_y.saturating_add(1);

        let followup_area = Rect {
            x: area.x,
            y: cursor_y,
            width: area.width,
            height: 1,
        };
        let followup_spans: Vec<Span<'static>> = vec![
            gutter(),
            "Press Esc to continue anyway and set them yourself, or provide them here and let Codex handle it.".into(),
        ];
        Paragraph::new(Line::from(followup_spans)).render(followup_area, buf);
        cursor_y = cursor_y.saturating_add(2);

        if cursor_y < area.y.saturating_add(area.height) {
            let input_hint_area = Rect {
                x: area.x,
                y: cursor_y,
                width: area.width,
                height: 1,
            };
            let step_hint = if self.dependencies.len() > 1 {
                format!(
                    " (step {step}/{total})",
                    step = self.current_index.saturating_add(1),
                    total = self.dependencies.len()
                )
            } else {
                String::new()
            };
            let input_hint_spans: Vec<Span<'static>> = vec![
                gutter(),
                format!(
                    "Enter a value for {name}{step_hint}.",
                    name = self.current_dependency_name(),
                    step_hint = step_hint
                )
                .into(),
            ];
            Paragraph::new(Line::from(input_hint_spans)).render(input_hint_area, buf);
            cursor_y = cursor_y.saturating_add(1);
        }

        if cursor_y < area.y.saturating_add(area.height) {
            let dep_area = Rect {
                x: area.x,
                y: cursor_y,
                width: area.width,
                height: 1,
            };
            if let Some(dependency) = self.dependencies.get(self.current_index) {
                let mut spans: Vec<Span<'static>> = vec![
                    gutter(),
                    "- ".into(),
                    dependency.name.clone().green().bold(),
                ];
                if let Some(description) = &dependency.description {
                    spans.push(" - ".dim());
                    spans.push(description.clone().dim());
                }
                Paragraph::new(Line::from(spans)).render(dep_area, buf);
                cursor_y = cursor_y.saturating_add(1);
            }
        }

        let input_area = Rect {
            x: area.x,
            y: cursor_y,
            width: area.width,
            height: input_height,
        };
        if input_area.width >= 2 {
            for row in 0..input_area.height {
                Paragraph::new(Line::from(vec![gutter()])).render(
                    Rect {
                        x: input_area.x,
                        y: input_area.y.saturating_add(row),
                        width: 2,
                        height: 1,
                    },
                    buf,
                );
            }

            let text_area_height = input_area.height.saturating_sub(1);
            if text_area_height > 0 {
                if input_area.width > 2 {
                    let blank_rect = Rect {
                        x: input_area.x.saturating_add(2),
                        y: input_area.y,
                        width: input_area.width.saturating_sub(2),
                        height: 1,
                    };
                    Clear.render(blank_rect, buf);
                }
                let textarea_rect = Rect {
                    x: input_area.x.saturating_add(2),
                    y: input_area.y.saturating_add(1),
                    width: input_area.width.saturating_sub(2),
                    height: text_area_height,
                };
                let mut state = self.textarea_state.borrow_mut();
                StatefulWidgetRef::render_ref(&(&self.textarea), textarea_rect, buf, &mut state);
                if self.textarea.text().is_empty() {
                    Paragraph::new(Line::from("Value".dim())).render(textarea_rect, buf);
                }
            }
        }

        let hint_blank_y = input_area.y.saturating_add(input_height);
        if hint_blank_y < area.y.saturating_add(area.height) {
            let blank_area = Rect {
                x: area.x,
                y: hint_blank_y,
                width: area.width,
                height: 1,
            };
            Clear.render(blank_area, buf);
        }

        let hint_y = hint_blank_y.saturating_add(1);
        if hint_y < area.y.saturating_add(area.height) {
            Paragraph::new(dependency_popup_hint_line()).render(
                Rect {
                    x: area.x,
                    y: hint_y,
                    width: area.width,
                    height: 1,
                },
                buf,
            );
        }
    }

    fn cursor_pos(&self, area: Rect) -> Option<(u16, u16)> {
        if area.height < 2 || area.width <= 2 {
            return None;
        }
        let input_height = self.input_height(area.width);
        let input_y = area.y.saturating_add(5);
        let text_area_height = input_height.saturating_sub(1);
        if text_area_height == 0 {
            return None;
        }
        let text_area = Rect {
            x: area.x.saturating_add(2),
            y: input_y.saturating_add(1),
            width: area.width.saturating_sub(2),
            height: text_area_height,
        };
        let state = self.textarea_state.borrow();
        self.textarea.cursor_pos_with_state(text_area, *state)
    }
}

impl DependencyInputView {
    fn input_height(&self, width: u16) -> u16 {
        let usable_width = width.saturating_sub(2);
        let text_height = self.textarea.desired_height(usable_width).clamp(1, 5);
        text_height.saturating_add(1).min(6)
    }

    fn current_dependency_name(&self) -> String {
        self.dependencies
            .get(self.current_index)
            .map(|dependency| dependency.name.clone())
            .unwrap_or_else(|| "the next value".to_string())
    }
}

fn gutter() -> Span<'static> {
    "  ".into()
}

fn dependency_popup_hint_line() -> Line<'static> {
    Line::from(vec![
        "Press ".into(),
        key_hint::plain(KeyCode::Enter).into(),
        " to provide it here, or ".into(),
        key_hint::plain(KeyCode::Esc).into(),
        " to exit and set it manually.".into(),
    ])
}
