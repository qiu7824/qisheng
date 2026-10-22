//! Inert HTML5 body text projection with a fixed tree and mutation budget.
use super::ControlResult;
use super::protocol::invalid;
use html5ever::tendril::{StrTendril, TendrilSink};
use html5ever::tree_builder::{ElementFlags, NodeOrText, QuirksMode, TreeSink};
use html5ever::{Attribute, ExpandedName, QualName};
use std::borrow::Cow;

const MAX_INPUT: usize = 4096;
const MAX_NODES: usize = 1024;
const MAX_TEXT: usize = 16 * 1024;
const MAX_OPERATIONS: usize = 32 * 1024;
const HTML_NS: &str = "http://www.w3.org/1999/xhtml";

#[derive(Clone)]
struct Handle {
    id: usize,
    name: Option<QualName>,
    template: Option<usize>,
    math_integration: bool,
}
struct Node {
    name: Option<QualName>,
    text: Option<String>,
    parent: Option<usize>,
    children: Vec<usize>,
}
struct TextTree {
    nodes: Vec<Node>,
    next_id: usize,
    text_bytes: usize,
    operations: usize,
    failed: bool,
}
impl TextTree {
    fn new() -> Self {
        Self {
            nodes: vec![Node {
                name: None,
                text: None,
                parent: None,
                children: Vec::new(),
            }],
            next_id: 1,
            text_bytes: 0,
            operations: 0,
            failed: false,
        }
    }
    fn operation(&mut self) -> bool {
        self.operations += 1;
        if self.operations > MAX_OPERATIONS {
            self.failed = true;
        }
        !self.failed
    }
    fn allocate(&mut self, name: Option<QualName>, text: Option<String>) -> Handle {
        let id = self.next_id;
        self.next_id += 1;
        if id >= MAX_NODES || self.failed {
            self.failed = true;
        } else {
            self.nodes.push(Node {
                name: name.clone(),
                text,
                parent: None,
                children: Vec::new(),
            });
        }
        // Handles preserve the actual element name even after budget exhaustion.
        // The tokenizer is stopped at the next scalar boundary; virtual handles
        // never grow the owned tree or produce a partial successful projection.
        Handle {
            id,
            name,
            template: None,
            math_integration: false,
        }
    }
    fn detach(&mut self, id: usize) {
        if !self.operation() || id >= self.nodes.len() {
            return;
        }
        if let Some(parent) = self.nodes[id].parent.take() {
            self.nodes[parent].children.retain(|child| *child != id);
        }
    }
    fn insert(&mut self, parent: usize, child: NodeOrText<Handle>, before: Option<usize>) {
        if !self.operation() || parent >= self.nodes.len() {
            return;
        }
        let id = match child {
            NodeOrText::AppendNode(handle) => {
                if handle.id >= self.nodes.len() {
                    self.failed = true;
                    return;
                }
                handle.id
            }
            NodeOrText::AppendText(text) => {
                if self.text_bytes + text.len() > MAX_TEXT {
                    self.failed = true;
                    return;
                }
                self.text_bytes += text.len();
                let handle = self.allocate(None, Some(text.to_string()));
                if self.failed {
                    return;
                }
                handle.id
            }
        };
        self.detach(id);
        if self.failed {
            return;
        }
        let position = before.and_then(|before| {
            self.nodes[parent]
                .children
                .iter()
                .position(|child| *child == before)
        });
        if let Some(position) = position {
            self.nodes[parent].children.insert(position, id);
        } else {
            self.nodes[parent].children.push(id);
        }
        self.nodes[id].parent = Some(parent);
    }
    fn body_text(self) -> ControlResult<String> {
        if self.failed {
            return Err(invalid("HTML metadata exceeds parser budget"));
        }
        let html = self.nodes[0].children.iter().copied().find(|id| {
            self.nodes[*id]
                .name
                .as_ref()
                .is_some_and(|name| name.ns.as_ref() == HTML_NS && name.local.as_ref() == "html")
        });
        let body = html.and_then(|html| {
            self.nodes[html].children.iter().copied().find(|id| {
                self.nodes[*id].name.as_ref().is_some_and(|name| {
                    name.ns.as_ref() == HTML_NS
                        && ["body", "frameset"].contains(&name.local.as_ref())
                })
            })
        });
        let Some(body) = body else {
            return Ok(String::new());
        };
        let mut output = String::new();
        let mut pending = vec![body];
        let mut visited = 0;
        while let Some(id) = pending.pop() {
            visited += 1;
            if visited > MAX_NODES {
                return Err(invalid("Invalid HTML metadata tree"));
            }
            let node = &self.nodes[id];
            if let Some(text) = &node.text {
                output.push_str(text);
            }
            pending.extend(node.children.iter().rev().copied());
        }
        Ok(output)
    }
}
impl TreeSink for TextTree {
    type Handle = Handle;
    type Output = Self;
    fn finish(self) -> Self {
        self
    }
    fn parse_error(&mut self, _: Cow<'static, str>) {
        self.operation();
    }
    fn get_document(&mut self) -> Handle {
        Handle {
            id: 0,
            name: None,
            template: None,
            math_integration: false,
        }
    }
    fn elem_name<'a>(&'a self, target: &'a Handle) -> ExpandedName<'a> {
        target
            .name
            .as_ref()
            .expect("HTML parser requested a non-element name")
            .expanded()
    }
    fn create_element(&mut self, name: QualName, _: Vec<Attribute>, flags: ElementFlags) -> Handle {
        self.operation();
        let mut handle = self.allocate(Some(name), None);
        if flags.template {
            handle.template = Some(self.allocate(None, None).id);
        }
        handle.math_integration = flags.mathml_annotation_xml_integration_point;
        handle
    }
    fn create_comment(&mut self, _: StrTendril) -> Handle {
        self.operation();
        self.allocate(None, None)
    }
    fn create_pi(&mut self, _: StrTendril, _: StrTendril) -> Handle {
        self.operation();
        self.allocate(None, None)
    }
    fn append(&mut self, parent: &Handle, child: NodeOrText<Handle>) {
        self.insert(parent.id, child, None);
    }
    fn append_based_on_parent_node(
        &mut self,
        element: &Handle,
        previous: &Handle,
        child: NodeOrText<Handle>,
    ) {
        if element.id < self.nodes.len() && self.nodes[element.id].parent.is_some() {
            self.append_before_sibling(element, child);
        } else {
            self.append(previous, child);
        }
    }
    fn append_doctype_to_document(&mut self, _: StrTendril, _: StrTendril, _: StrTendril) {
        self.operation();
    }
    fn get_template_contents(&mut self, target: &Handle) -> Handle {
        Handle {
            id: target
                .template
                .expect("HTML parser requested non-template contents"),
            name: None,
            template: None,
            math_integration: false,
        }
    }
    fn same_node(&self, left: &Handle, right: &Handle) -> bool {
        left.id == right.id
    }
    fn set_quirks_mode(&mut self, _: QuirksMode) {}
    fn append_before_sibling(&mut self, sibling: &Handle, new: NodeOrText<Handle>) {
        if let Some(parent) = self.nodes.get(sibling.id).and_then(|node| node.parent) {
            self.insert(parent, new, Some(sibling.id));
        } else {
            self.operation();
        }
    }
    fn add_attrs_if_missing(&mut self, _: &Handle, _: Vec<Attribute>) {
        self.operation();
    }
    fn remove_from_parent(&mut self, target: &Handle) {
        self.detach(target.id);
    }
    fn reparent_children(&mut self, node: &Handle, new_parent: &Handle) {
        if !self.operation() || node.id >= self.nodes.len() || new_parent.id >= self.nodes.len() {
            return;
        }
        let children = std::mem::take(&mut self.nodes[node.id].children);
        for child in children {
            self.nodes[child].parent = Some(new_parent.id);
            self.nodes[new_parent.id].children.push(child);
        }
    }
    fn is_mathml_annotation_xml_integration_point(&self, target: &Handle) -> bool {
        target.math_integration
    }
}
pub(super) fn body_text(input: &str) -> ControlResult<String> {
    if input.len() > MAX_INPUT {
        return Err(invalid("HTML metadata exceeds input budget"));
    }
    let options = html5ever::ParseOpts {
        tree_builder: html5ever::tree_builder::TreeBuilderOpts {
            scripting_enabled: false,
            drop_doctype: true,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut parser = html5ever::parse_document(TextTree::new(), options);
    for scalar in input.chars() {
        parser.process(scalar.to_string().into());
        if parser.tokenizer.sink.sink.failed {
            return Err(invalid("HTML metadata exceeds parser budget"));
        }
    }
    parser.finish().body_text()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parser_budgets_return_an_error_without_partial_text() {
        assert!(body_text(&"x".repeat(MAX_INPUT + 1)).is_err());
        assert!(body_text(&format!("{}visible", "<b>".repeat(MAX_NODES))).is_err());
    }
}
