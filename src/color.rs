use log::info;
use ropey::Rope;
use std::collections::HashSet;
use std::sync::Arc;
use tokio::time::Instant;
use tower_lsp::lsp_types::*;

use crate::completion::{estimate_constraint_env, find_section, CompletionEnv, Section};
use crate::parse::parse_path;
use crate::semantic::RootGraph;
use crate::util::node_range;
use crate::{filegraph::*, util::lsp_range};
use tree_sitter::{Node, QueryCursor, Tree};

#[derive(Clone, Debug, PartialEq, Eq)]
struct AbsToken {
    range: Range,
    kind: u32,
}
struct FileState {
    state: Vec<SemanticToken>,
}
pub fn token_types() -> Vec<SemanticTokenType> {
    vec![
        SemanticTokenType::KEYWORD,
        SemanticTokenType::OPERATOR,
        SemanticTokenType::NAMESPACE,
        SemanticTokenType::ENUM_MEMBER,
        SemanticTokenType::CLASS,
        SemanticTokenType::COMMENT,
        SemanticTokenType::ENUM,
        SemanticTokenType::INTERFACE,
        SemanticTokenType::FUNCTION,
        SemanticTokenType::MACRO,
        SemanticTokenType::PARAMETER,
    ]
}
fn token_index(name: &str) -> u32 {
    match name {
        "keyword" => 0,
        "operator" => 1,
        "namespace" => 2,
        "enumMember" => 3,
        "class" => 4,
        "comment" => 5,
        "enum" => 6,
        "interface" => 7,
        "function" => 8,
        "macro" => 9,
        "parameter" => 10,
        _ => 0,
    }
}

pub enum ColorUpdate {
    File(Tree),
    Root(RootGraph),
}
fn fast_lsp_range(
    node: Node,
    source: &Rope,
    utf16_lines: &HashSet<usize>,
) -> tower_lsp::lsp_types::Range {
    if utf16_lines.contains(&node.start_position().row)
        || utf16_lines.contains(&node.end_position().row)
    {
        node_range(node, source)
    } else {
        tower_lsp::lsp_types::Range {
            start: Position {
                line: node.start_position().row as u32,
                character: node.start_position().column as u32,
            },
            end: Position {
                line: node.end_position().row as u32,
                character: node.end_position().column as u32,
            },
        }
    }
}

impl FileState {
    fn diff(&self, new: &FileState) -> SemanticTokensFullDeltaResult {
        //todo use a proper diffing algorithm
        let prefix = self
            .state
            .iter()
            .zip(new.state.iter())
            .take_while(|(i, j)| i == j)
            .count();
        let diff = self.state.len().abs_diff(new.state.len());
        if self.state.len() < new.state.len() {
            if self.state[prefix..]
                .iter()
                .zip(new.state[prefix + diff..].iter())
                .all(|(i, k)| i == k)
            {
                return SemanticTokensFullDeltaResult::TokensDelta(SemanticTokensDelta {
                    result_id: None,
                    edits: vec![SemanticTokensEdit {
                        start: prefix as u32,
                        delete_count: 0,
                        data: Some(new.state[prefix..prefix + diff].to_vec()),
                    }],
                });
            }
        } else if self.state.len() > new.state.len() {
            if self.state[prefix + diff..]
                .iter()
                .zip(new.state[prefix..].iter())
                .all(|(i, k)| i == k)
            {
                return SemanticTokensFullDeltaResult::TokensDelta(SemanticTokensDelta {
                    result_id: None,
                    edits: vec![SemanticTokensEdit {
                        start: prefix as u32,
                        delete_count: diff as u32,
                        data: None,
                    }],
                });
            }
        }
        SemanticTokensFullDeltaResult::TokensDelta(SemanticTokensDelta {
            result_id: None,
            edits: vec![SemanticTokensEdit {
                start: prefix as u32,
                delete_count: (self.state.len() - prefix) as u32,
                data: Some(new.state[prefix..].to_vec()),
            }],
        })
    }
    fn new(origin:& Url, tree: Tree, source: &ropey::Rope, root: &RootGraph) -> Self {
        let time = Instant::now();
        let mut cursor = QueryCursor::new();
        let mut token = vec![];
        let mut utf16_line = HashSet::new();
        for (i, line) in source.lines().enumerate() {
            for c in line.chars() {
                if c.len_utf8() != c.len_utf16() || c.len_utf8() != 1 {
                    utf16_line.insert(i);
                }
            }
        }
        let captures = TS.queries.highlight.capture_names();
        let file = &root.files[&origin.as_str().into()];
        for i in cursor.matches(
            &TS.queries.highlight,
            tree.root_node(),
            crate::util::node_source(source),
        ) {
            for c in i.captures {
                let kind = captures[c.index as usize].as_str();
                if kind == "some_path" {
                    let section = find_section(c.node);
                    match section {
                        Section::Constraints => {
                            let env = estimate_constraint_env(c.node, None, source);
                            match env {
                                CompletionEnv::Numeric => {
                                    let path = parse_path(c.node, source).unwrap();
                                    if let Some(attrib) = root
                                        .resolve(file.name, &path.names)
                                        .find(|node| matches!(node.sym, Symbol::Number(..)))
                                    {
                                        let mut sym = Some(attrib.sym);
                                        for i in (0..path.names.len()).rev() {
                                            if let Some(cur) = sym {
                                                token.push(AbsToken {
                                                    range: lsp_range(path.spans[i].clone(),source).unwrap(),
                                                    kind: token_index("enumMember"),
                                                });
                                                let next  = root.files[&attrib.file].owner(cur);
                                                if next.is_value(){
                                                    sym = Some(next);
                                                }
                                                else{
                                                    sym = None;
                                                }
                                            } else {
                                                token.push(AbsToken {
                                                    range: lsp_range(path.spans[i].clone(),source).unwrap(),
                                                    kind: token_index("parameter"),
                                                });
                                            }
                                        }
                                    } else {
                                        token.push(AbsToken {
                                            range: fast_lsp_range(c.node, source, &utf16_line),
                                            kind: token_index("parameter"),
                                        });
                                    };
                                }
                                _ => {
                                    token.push(AbsToken {
                                        range: fast_lsp_range(c.node, source, &utf16_line),
                                        kind: token_index("parameter"),
                                    });
                                }
                            }
                        }
                        _ => token.push(AbsToken {
                            range: fast_lsp_range(c.node, source, &utf16_line),
                            kind: token_index("parameter"),
                        }),
                    }
                } else {
                    let range = fast_lsp_range(c.node, source, &utf16_line);
                    token.push(AbsToken {
                        range,
                        kind: token_index(kind),
                    });
                }
            }
        }
        token.sort_by_key(|a| (a.range.start.line, a.range.start.character));
        token.dedup();
        let mut filtered = Vec::new();
        let mut last: Option<AbsToken> = None;
        for i in token.iter() {
            if let Some(last) = last.as_ref() {
                if last.range.end.line > i.range.start.line {
                    continue;
                }
                if last.range.end.line == i.range.start.line
                    && last.range.end.character > i.range.start.character
                {
                    continue;
                }
            }
            if i.range.start.line == i.range.end.line {
                let next_col = i.range.start.character;
                let next_line = i.range.start.line;
                let len = i.range.end.character - i.range.start.character;
                if let Some(last) = last.as_ref() {
                    let last_line = last.range.end.line;
                    let last_col = last.range.start.character;
                    filtered.push(SemanticToken {
                        delta_line: next_line - last_line,
                        delta_start: if next_line == last_line {
                            next_col - last_col
                        } else {
                            next_col
                        },
                        length: len,
                        token_type: i.kind,
                        token_modifiers_bitset: 0,
                    })
                } else {
                    filtered.push(SemanticToken {
                        delta_line: next_line,
                        delta_start: next_col,
                        length: len,
                        token_type: i.kind,
                        token_modifiers_bitset: 0,
                    })
                }
            } else {
                let next_col = i.range.start.character;
                let next_line = i.range.start.line;
                if let Some(last) = last.as_ref() {
                    let last_line = last.range.end.line;
                    let last_col = last.range.start.character;
                    filtered.push(SemanticToken {
                        delta_line: next_line - last_line,
                        delta_start: if next_line == last_line {
                            next_col - last_col
                        } else {
                            next_col
                        },
                        length: source.line(i.range.start.line as usize).len_utf16_cu() as u32
                            - next_col,
                        token_type: i.kind,
                        token_modifiers_bitset: 0,
                    })
                } else {
                    filtered.push(SemanticToken {
                        delta_line: next_line,
                        delta_start: next_col,
                        length: source.line(i.range.start.line as usize).len_utf16_cu() as u32
                            - next_col,
                        token_type: i.kind,
                        token_modifiers_bitset: 0,
                    })
                }
                if i.range.start.line - i.range.end.line > 2 {
                    for l in i.range.start.line + 1..i.range.end.line - 1 {
                        filtered.push(SemanticToken {
                            delta_line: 1,
                            delta_start: 0,
                            length: source.line(l as usize).len_utf16_cu() as u32,
                            token_type: i.kind,
                            token_modifiers_bitset: 0,
                        })
                    }
                }
                filtered.push(SemanticToken {
                    delta_line: 1,
                    delta_start: 0,
                    length: i.range.end.character,
                    token_type: i.kind,
                    token_modifiers_bitset: 0,
                })
            }
            last = Some(i.clone());
        }

        info!("Semantic highlight took {:?}", time.elapsed());
        FileState { state: filtered }
    }
}
pub struct State {
    files: dashmap::DashMap<Url, FileState>,
}
impl State {
    pub fn new() -> Self {
        State {
            files: Default::default(),
        }
    }
    pub fn get(
        &self,
        root: RootGraph,
        uri: Url,
        tree: Tree,
        source: ropey::Rope,
    ) -> SemanticTokens {
        let state = FileState::new(&uri, tree, &source, &root);
        let out = state.state.clone();
        self.files.insert(uri.clone(), state);
        SemanticTokens {
            result_id: None,
            data: out,
        }
    }
    pub fn delta(
        &self,
        root: RootGraph,
        uri: Url,
        tree: Tree,
        source: ropey::Rope,
    ) -> SemanticTokensFullDeltaResult {
        if let Some(old) = self.files.get(&uri) {
            let state = FileState::new(&uri, tree, &source, &root);
            let diff = old.diff(&state);
            self.files.insert(uri.clone(), state);
            diff
        } else {
            let state = FileState::new(&uri, tree, &source, &root);
            let out = state.state.clone();
            self.files.insert(uri.clone(), state);
            SemanticTokensFullDeltaResult::Tokens(SemanticTokens {
                result_id: None,
                data: out,
            })
        }
    }
}
