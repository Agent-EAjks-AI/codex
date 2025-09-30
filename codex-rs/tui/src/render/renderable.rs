use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::widgets::Paragraph;
use ratatui::widgets::WidgetRef;

pub trait Renderable {
    fn render(&self, area: Rect, buf: &mut Buffer);
    fn desired_height(&self, width: u16) -> u16;
}

impl Renderable for () {
    fn render(&self, _area: Rect, _buf: &mut Buffer) {}
    fn desired_height(&self, _width: u16) -> u16 {
        0
    }
}

impl Renderable for &str {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        self.render_ref(area, buf);
    }
    fn desired_height(&self, _width: u16) -> u16 {
        1
    }
}

impl Renderable for String {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        self.render_ref(area, buf);
    }
    fn desired_height(&self, _width: u16) -> u16 {
        1
    }
}

impl<'a> Renderable for Line<'a> {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        WidgetRef::render_ref(self, area, buf);
    }
    fn desired_height(&self, _width: u16) -> u16 {
        1
    }
}

impl<'a> Renderable for Paragraph<'a> {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        self.render_ref(area, buf);
    }
    fn desired_height(&self, width: u16) -> u16 {
        self.line_count(width) as u16
    }
}

impl<R: Renderable> Renderable for Option<R> {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        if let Some(renderable) = self {
            renderable.render(area, buf);
        }
    }

    fn desired_height(&self, width: u16) -> u16 {
        if let Some(renderable) = self {
            renderable.desired_height(width)
        } else {
            0
        }
    }
}

pub struct ColumnRenderable {
    children: Vec<Box<dyn Renderable>>,
}

impl Renderable for ColumnRenderable {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        let mut y = area.y;
        for child in &self.children {
            let child_area = Rect::new(area.x, y, area.width, child.desired_height(area.width));
            child.render(child_area, buf);
            y += child_area.height;
        }
    }

    fn desired_height(&self, width: u16) -> u16 {
        self.children
            .iter()
            .map(|child| child.desired_height(width))
            .sum()
    }
}

impl ColumnRenderable {
    pub fn new(children: impl IntoIterator<Item = Box<dyn Renderable>>) -> Self {
        Self {
            children: children.into_iter().collect(),
        }
    }
}

/*
use std::ops::Range;

use ratatui::layout::Size;

#[derive(Clone, Debug)]
pub struct BoxConstraints {
    pub width: Range<Option<u16>>,
    pub height: Range<Option<u16>>,
}

impl BoxConstraints {
    pub fn tight(size: Size) -> Self {
        Self {
            width: Some(size.width)..Some(size.width),
            height: Some(size.height)..Some(size.height),
        }
    }
    pub fn loose(size: Size) -> Self {
        Self {
            width: None..Some(size.width),
            height: None..Some(size.height),
        }
    }

    pub fn tight_for(width: Option<u16>, height: Option<u16>) -> Self {
        Self {
            width: width..width,
            height: height..height,
        }
    }
}

trait Renderable {
    fn render(&self, area: Rect, buf: &mut Buffer);
    fn layout(&self, constraints: BoxConstraints) -> Rect;
}

pub struct Column {
    pub children: Vec<Box<dyn Renderable>>,
}

impl Renderable for Column {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        for child in &self.children {
            child.render(area, buf);
        }
    }

    fn layout(&self, constraints: BoxConstraints) -> Rect {
        for child in &self.children {
            child.layout(constraints.clone());
        }
        Rect::new(0, 0, 0, 0)
    }
} */
