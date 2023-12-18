mod util;
mod uni_codes;
mod ast_tree;
mod filter_parser;
mod element_processor;

#[macro_use]
extern crate lazy_static;

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;
use std::thread::current;
use lazy_static::lazy_static;
use regex::Regex;
use rs_html_parser::{Parser, ParserOptions};
use rs_html_parser_tokenizer::TokenizerOptions;
use rs_html_parser_tokens::{Token, TokenKind};
use unicase::Ascii;
use crate::ast_tree::{ASTElement, ASTNode, ASTTree, create_ast_element};
use crate::element_processor::process_element;
use crate::uni_codes::{UC_TYPE, UC_V_FOR};
use crate::util::{get_attribute, has_attribute};

lazy_static! {
    static ref INVALID_ATTRIBUTE_RE: Regex = Regex::new(r##"/[\s"'<>\/=]/"##).unwrap();
    static ref FOR_ALIAS_RE: Regex = Regex::new(r"([\s\S]*?)\s+(?:in|of)\s+([\s\S]*)").unwrap();
    static ref FOR_ITERATOR_RE: Regex = Regex::new(r",([^,\}\]]*)(?:,([^,\}\]]*))?$").unwrap();
    static ref STRIP_PARENS_RE: Regex = Regex::new(r"^\(|\)$").unwrap();
    static ref DYNAMIC_ARG_RE: Regex = Regex::new(r"^\[.*\]$").unwrap();
    static ref ARG_RE: Regex = Regex::new(r":(.*)$").unwrap();
    static ref BIND_RE: Regex = Regex::new(r"^:|^\.|^v-bind:").unwrap();
    static ref PROP_BIND_RE: Regex = Regex::new(r"^\.").unwrap();
    static ref MODIFIER_RE: Regex = Regex::new(r"\.[^.\]]+(?=[^\]]*$)").unwrap();
    static ref SLOT_RE: Regex = Regex::new(r"^v-slot(:|$)|^#").unwrap();
    static ref LINE_BREAK_RE: Regex = Regex::new(r"[\r\n]").unwrap();
    static ref WHITESPACE_RE: Regex = Regex::new(r"[ \f\t\r\n]+").unwrap();
}


// TODO: Move to options
fn warn(message: &str) {
    println!("{}", message)
}

struct CompilerOptions {
    dev: bool,
    is_ssr: bool,

    is_pre_tag: Option<fn(tag: &str) -> bool>
}


fn is_forbidden_tag(el: &Token) -> bool {
    if &el.kind != &TokenKind::OpenTag {
        return false;
    }

    match &*el.data {
        "style" => true,
        "script" => {
            let attr_value = get_attribute(el, &UC_TYPE);

            if let Some((val, _quote)) = attr_value {
                return &**val == "text/javascript";
            }

            return false;
        }
        _ => false
    }
}

pub struct VueParser {
    options: CompilerOptions,

    in_v_pre: bool,
    in_pre: bool,
    warned: bool,
}

const PARSER_OPTIONS: ParserOptions = ParserOptions {
    xml_mode: false,
    tokenizer_options: TokenizerOptions {
    xml_mode: Some(false),
    decode_entities: Some(true),
    },
};

impl VueParser {
    pub fn new(options: CompilerOptions) -> VueParser {
        VueParser {
            options,
            in_v_pre: false,
            in_pre: false,
            warned: false,
        }
    }

    fn warn_once(&mut self, msg: &str) {
        if !self.warned {
            self.warned = true;
            warn(msg);
        }
    }

    fn check_root_constraints(&mut self, new_root: &ASTElement ) {
        if self.warned {
           return;
        }

        if new_root.token.data.eq_ignore_ascii_case("slot")
            || new_root.token.data.eq_ignore_ascii_case("template") {
            self.warn_once("Cannot use <${el.tag}> as component root element because it may contain multiple nodes.")
        }
        if has_attribute(&new_root.token, &UC_V_FOR) {
            self.warn_once("Cannot use v-for on stateful component root element because it renders multiple elements.")
        }
    }

    fn platform_is_pre_tag(&mut self, tag: &str) -> bool {
        if let Some(pre_tag_fn) = self.options.is_pre_tag {
            return pre_tag_fn(tag);
        }

        return false;
    }

    pub fn parse(&mut self, template: &str) -> ASTTree {
        let parser = Parser::new(template, &PARSER_OPTIONS);
        let is_dev = self.options.dev;
        let mut root_tree: ASTTree = ASTTree::new(is_dev);
        let mut stack: VecDeque<usize> = VecDeque::new();
        let mut current_parent_id = 0;
        let mut is_root_set: bool = false;

        for token in parser {
            match token.kind {
                TokenKind::OpenTag => {
                    let node_rc = root_tree.create(
                        create_ast_element(token, is_dev),
                        current_parent_id
                    );
                    let mut node = node_rc.borrow_mut();

                     if is_dev {
                        if let Some(attrs) = &node.el.token.attrs {
                            for (attr_key, _attr_value) in attrs {
                                if INVALID_ATTRIBUTE_RE.find(&attr_key).is_some() {
                                    warn(
                                        "Invalid dynamic argument expression: attribute names cannot contain spaces, quotes, <, >, / or =."
                                    )
                                }
                            }
                        }
                    }

                    if is_forbidden_tag(&node.el.token) && !self.options.is_ssr {
                        node.el.forbidden = true;

                        if is_dev {
                            // TODO: add tag
                            warn("
            Templates should only be responsible for mapping the state to the
            UI. Avoid placing tags with side-effects in your templates, such as
            <{tag}> as they will not be parsed.
                ")
                        }
                    }

                    // TODO Apply pre-transforms

                    if !self.in_v_pre {
                        node.process_pre();
                        if node.el.pre {
                            self.in_v_pre = true;
                        }
                    }
                    if self.platform_is_pre_tag(&node.el.token.data) {
                        self.in_pre = true;
                    }
                    if self.in_v_pre {
                        node.process_raw_attributes()
                    } else if !node.el.processed {
                        node.process_for();
                        node.process_if();
                        node.process_once();
                    }

                    stack.push_back(node.id);
                },
                TokenKind::CloseTag => {
                    let current_open_tag_id = stack.pop_back();

                    if let Some(mut open_tag_id) = current_open_tag_id {
                        let mut node = root_tree.get(open_tag_id).unwrap().borrow_mut();
                        // trim white space ??

                        if !self.in_v_pre && !node.el.processed {
                            process_element(node);
                        }
                    }
                },
                TokenKind::Text => {

                }
                _ => {
                    todo!("missing implementation")
                }
            }
        }

        root_tree
    }
}
