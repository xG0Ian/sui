// Copyright (c) The Diem Core Contributors
// Copyright (c) The Move Contributors
// SPDX-License-Identifier: Apache-2.0

use crate::{
    context::Context,
    symbols::{
        self, mod_ident_to_ide_string, ret_type_to_ide_str, type_args_to_ide_string,
        type_list_to_ide_string, type_to_ide_string, CursorContext, CursorDefinition, DefInfo,
        FunType, PrecompiledPkgDeps, SymbolicatorRunner, Symbols,
    },
    utils,
};
use lsp_server::Request;
use lsp_types::{
    CompletionItem, CompletionItemKind, CompletionItemLabelDetails, CompletionParams,
    Documentation, InsertTextFormat, Position,
};
use move_command_line_common::files::FileHash;
use move_compiler::{
    editions::Edition,
    expansion::ast::{ModuleIdent_, Visibility},
    linters::LintLevel,
    naming::ast::{Type, Type_},
    parser::{
        ast::{self as P, Ability_},
        keywords::{BUILTINS, CONTEXTUAL_KEYWORDS, KEYWORDS, PRIMITIVE_TYPES},
        lexer::{Lexer, Tok},
    },
    shared::{ide::AutocompleteMethod, Identifier, Name},
};
use move_ir_types::location::Loc;
use move_symbol_pool::Symbol;

use std::{
    collections::{BTreeMap, HashSet},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};
use vfs::VfsPath;

/// Constructs an `lsp_types::CompletionItem` with the given `label` and `kind`.
fn completion_item(label: &str, kind: CompletionItemKind) -> CompletionItem {
    CompletionItem {
        label: label.to_owned(),
        kind: Some(kind),
        ..Default::default()
    }
}

/// Return a list of completion items corresponding to each one of Move's keywords.
///
/// Currently, this does not filter keywords out based on whether they are valid at the completion
/// request's cursor position, but in the future it ought to. For example, this function returns
/// all specification language keywords, but in the future it should be modified to only do so
/// within a spec block.
fn keywords() -> Vec<CompletionItem> {
    KEYWORDS
        .iter()
        .chain(CONTEXTUAL_KEYWORDS.iter())
        .chain(PRIMITIVE_TYPES.iter())
        .map(|label| {
            let kind = if label == &"copy" || label == &"move" {
                CompletionItemKind::OPERATOR
            } else {
                CompletionItemKind::KEYWORD
            };
            completion_item(label, kind)
        })
        .collect()
}

/// Return a list of completion items of Move's primitive types
fn primitive_types() -> Vec<CompletionItem> {
    PRIMITIVE_TYPES
        .iter()
        .map(|label| completion_item(label, CompletionItemKind::KEYWORD))
        .collect()
}

/// Return a list of completion items corresponding to each one of Move's builtin functions.
fn builtins() -> Vec<CompletionItem> {
    BUILTINS
        .iter()
        .map(|label| completion_item(label, CompletionItemKind::FUNCTION))
        .collect()
}

/// Lexes the Move source file at the given path and returns a list of completion items
/// corresponding to the non-keyword identifiers therein.
///
/// Currently, this does not perform semantic analysis to determine whether the identifiers
/// returned are valid at the request's cursor position. However, this list of identifiers is akin
/// to what editors like Visual Studio Code would provide as completion items if this language
/// server did not initialize with a response indicating it's capable of providing completions. In
/// the future, the server should be modified to return semantically valid completion items, not
/// simple textual suggestions.
fn identifiers(buffer: &str, symbols: &Symbols, path: &Path) -> Vec<CompletionItem> {
    // TODO thread through package configs
    let mut lexer = Lexer::new(buffer, FileHash::new(buffer), Edition::LEGACY);
    if lexer.advance().is_err() {
        return vec![];
    }
    let mut ids = HashSet::new();
    while lexer.peek() != Tok::EOF {
        // Some tokens, such as "phantom", are contextual keywords that are only reserved in
        // certain contexts. Since for now this language server doesn't analyze semantic context,
        // tokens such as "phantom" are always present in keyword suggestions. To avoid displaying
        // these keywords to the user twice in the case that the token "phantom" is present in the
        // source program (once as a keyword, and once as an identifier), we filter out any
        // identifier token that has the same text as a keyword.
        if lexer.peek() == Tok::Identifier && !KEYWORDS.contains(&lexer.content()) {
            // The completion item kind "text" indicates the item is not based on any semantic
            // context of the request cursor's position.
            ids.insert(lexer.content());
        }
        if lexer.advance().is_err() {
            break;
        }
    }

    let mods_opt = symbols.file_mods.get(path);

    // The completion item kind "text" indicates that the item is based on simple textual matching,
    // not any deeper semantic analysis.
    ids.iter()
        .map(|label| {
            if let Some(mods) = mods_opt {
                if mods
                    .iter()
                    .any(|m| m.functions().contains_key(&Symbol::from(*label)))
                {
                    completion_item(label, CompletionItemKind::FUNCTION)
                } else {
                    completion_item(label, CompletionItemKind::TEXT)
                }
            } else {
                completion_item(label, CompletionItemKind::TEXT)
            }
        })
        .collect()
}

/// Returns the token corresponding to the "trigger character" if it is one of `.`, `:`, '{', or
/// `::`. Otherwise, returns `None` (position points at the potential trigger character itself).
fn get_cursor_token(buffer: &str, position: &Position) -> Option<Tok> {
    let line = match buffer.lines().nth(position.line as usize) {
        Some(line) => line,
        None => return None, // Our buffer does not contain the line, and so must be out of date.
    };
    match line.chars().nth(position.character as usize) {
        Some('.') => Some(Tok::Period),
        Some(':') => {
            if position.character > 0
                && line.chars().nth(position.character as usize - 1) == Some(':')
            {
                Some(Tok::ColonColon)
            } else {
                Some(Tok::Colon)
            }
        }
        Some('{') => Some(Tok::LBrace),
        _ => None,
    }
}

/// Handle context-specific auto-completion requests with lbrace (`{`) trigger character.
fn context_specific_lbrace(
    symbols: &Symbols,
    cursor: &CursorContext,
) -> Option<Vec<CompletionItem>> {
    match &cursor.defn_name {
        // look for a struct definition on the line that contains `{`, check its abilities,
        // and do auto-completion if `key` ability is present
        Some(CursorDefinition::Struct(sname)) => {
            let mident = cursor.module?;
            let typed_ast = symbols.typed_ast.as_ref()?;
            let struct_def = typed_ast.info.struct_definition_opt(&mident, sname)?;
            if struct_def.abilities.has_ability_(Ability_::Key) {
                let obj_snippet = "\n\tid: UID,\n\t$1\n".to_string();
                let init_completion = CompletionItem {
                    label: "id: UID".to_string(),
                    kind: Some(CompletionItemKind::SNIPPET),
                    documentation: Some(Documentation::String("Object snippet".to_string())),
                    insert_text: Some(obj_snippet),
                    insert_text_format: Some(InsertTextFormat::SNIPPET),
                    ..Default::default()
                };
                vec![init_completion].into()
            } else {
                None
            }
        }
        Some(_) => None,
        None => None,
    }
}

fn fun_def_info(symbols: &Symbols, mod_ident: ModuleIdent_, name: Symbol) -> Option<&DefInfo> {
    let Some(mdef) = symbols
        .file_mods
        .values()
        .flatten()
        .find(|mdef| mdef.ident == mod_ident)
    else {
        return None;
    };

    let Some(fdef) = mdef.functions.get(&name) else {
        return None;
    };
    symbols.def_info(&fdef.name_loc)
}

fn lamda_snippet(sp!(_, ty): &Type, snippet_idx: &mut i32) -> Option<String> {
    if let Type_::Fun(vec, _) = ty {
        let arg_snippets = vec
            .iter()
            .map(|_| {
                *snippet_idx += 1;
                format!("${{{snippet_idx}}}")
            })
            .collect::<Vec<_>>()
            .join(", ");
        *snippet_idx += 1;
        return Some(format!("|{arg_snippets}| ${{{snippet_idx}}}"));
    }
    None
}

fn call_completion_item(
    mod_ident: &ModuleIdent_,
    fun_type: &FunType,
    method_name: &Symbol,
    function_name: &Symbol,
    type_args: &[Type],
    arg_names: &[Name],
    arg_types: &[Type],
    ret_type: &Type,
) -> CompletionItem {
    let sig_string = format!(
        "fun {}({}){}",
        type_args_to_ide_string(type_args, /* verbose */ false),
        type_list_to_ide_string(arg_types, /* verbose */ false),
        ret_type_to_ide_str(ret_type, /* verbose */ false)
    );
    // we omit the first argument which is guaranteed to be there
    // as this is a method and needs a receiver
    let mut snippet_idx = 0;
    let arg_snippet = arg_names
        .iter()
        .zip(arg_types)
        .skip(1)
        .map(|(name, ty)| {
            lamda_snippet(ty, &mut snippet_idx).unwrap_or_else(|| {
                let mut arg_name = name.to_string();
                if arg_name.starts_with('$') {
                    arg_name = arg_name[1..].to_string();
                }
                snippet_idx += 1;
                format!("${{{}:{}}}", snippet_idx, arg_name)
            })
        })
        .collect::<Vec<_>>()
        .join(", ");
    let macro_suffix = if matches!(fun_type, FunType::Macro) {
        "!"
    } else {
        ""
    };
    let label_details = Some(CompletionItemLabelDetails {
        detail: Some(format!(
            " ({}::{})",
            mod_ident_to_ide_string(mod_ident),
            function_name
        )),
        description: Some(sig_string),
    });

    CompletionItem {
        label: format!("{}{}()", method_name, macro_suffix),
        label_details,
        kind: Some(CompletionItemKind::METHOD),
        insert_text: Some(format!("{}{}({})", method_name, macro_suffix, arg_snippet)),
        insert_text_format: Some(InsertTextFormat::SNIPPET),
        ..Default::default()
    }
}

/// Handle dot auto-completion at a given position.
fn dot(symbols: &Symbols, use_fpath: &Path, position: &Position) -> Vec<CompletionItem> {
    let mut completions = vec![];
    let Some(fhash) = symbols.file_hash(use_fpath) else {
        eprintln!("no dot completions due to missing file");
        return completions;
    };
    let Some(loc) = utils::lsp_position_to_loc(&symbols.files, fhash, position) else {
        eprintln!("no dot completions due to missing loc");
        return completions;
    };
    let Some(info) = symbols.compiler_info.get_autocomplete_info(fhash, &loc) else {
        eprintln!("no dot completions due to no compiler autocomplete info");
        return completions;
    };
    for AutocompleteMethod {
        method_name,
        target_function: (mod_ident, function_name),
    } in &info.methods
    {
        let call_completion = if let Some(DefInfo::Function(
            ..,
            fun_type,
            _,
            type_args,
            arg_names,
            arg_types,
            ret_type,
            _,
        )) = fun_def_info(symbols, mod_ident.value, function_name.value())
        {
            call_completion_item(
                &mod_ident.value,
                fun_type,
                method_name,
                &function_name.value(),
                type_args,
                arg_names,
                arg_types,
                ret_type,
            )
        } else {
            // this shouldn't really happen as we should be able to get
            // `DefInfo` for a function but if for some reason we cannot,
            // let's generate simpler autotompletion value
            eprintln!("incomplete dot item");
            CompletionItem {
                label: format!("{method_name}()"),
                kind: Some(CompletionItemKind::METHOD),
                insert_text: Some(method_name.to_string()),
                insert_text_format: Some(InsertTextFormat::PLAIN_TEXT),
                ..Default::default()
            }
        };
        completions.push(call_completion);
    }
    for (n, t) in &info.fields {
        let label_details = Some(CompletionItemLabelDetails {
            detail: None,
            description: Some(type_to_ide_string(t, /* verbose */ false)),
        });
        let init_completion = CompletionItem {
            label: n.to_string(),
            label_details,
            kind: Some(CompletionItemKind::FIELD),
            insert_text: Some(n.to_string()),
            insert_text_format: Some(InsertTextFormat::PLAIN_TEXT),
            ..Default::default()
        };
        completions.push(init_completion);
    }
    completions
}

/// Handle context-specific auto-completion requests with no trigger character.
fn context_specific_no_trigger(
    symbols: &Symbols,
    use_fpath: &Path,
    buffer: &str,
    position: &Position,
) -> (Vec<CompletionItem>, bool) {
    eprintln!("looking for dot");
    let mut completions = dot(symbols, use_fpath, position);
    eprintln!("dot found: {}", !completions.is_empty());
    if !completions.is_empty() {
        // found dot completions - do not look for any other
        return (completions, true);
    }

    let mut only_custom_items = false;

    let strings = preceding_strings(buffer, position);

    if strings.is_empty() {
        return (completions, only_custom_items);
    }

    // at this point only try to auto-complete init function declararation - get the last string
    // and see if it represents the beginning of init function declaration
    const INIT_FN_NAME: &str = "init";
    let (n, use_col) = strings.last().unwrap();
    for u in symbols.line_uses(use_fpath, position.line) {
        if *use_col >= u.col_start() && *use_col <= u.col_end() {
            let def_loc = u.def_loc();
            let Some(use_file_mod_definition) = symbols.file_mods.get(use_fpath) else {
                break;
            };
            let Some(use_file_mod_def) = use_file_mod_definition.first() else {
                break;
            };
            if is_definition(
                symbols,
                position.line,
                u.col_start(),
                use_file_mod_def.fhash(),
                def_loc,
            ) {
                // since it's a definition, there is no point in trying to suggest a name
                // if one is about to create a fresh identifier
                only_custom_items = true;
            }
            let Some(def_info) = symbols.def_info(&def_loc) else {
                break;
            };
            let DefInfo::Function(mod_ident, v, ..) = def_info else {
                // not a function
                break;
            };
            if !INIT_FN_NAME.starts_with(n) {
                // starting to type "init"
                break;
            }
            if !matches!(v, Visibility::Internal) {
                // private (otherwise perhaps it's "init_something")
                break;
            }

            // get module info containing the init function
            let Some(mdef) = symbols.mod_defs(&u.def_loc().file_hash(), *mod_ident) else {
                break;
            };

            if mdef.functions().contains_key(&(INIT_FN_NAME.into())) {
                // already has init function
                break;
            }

            let sui_ctx_arg = "ctx: &mut TxContext";

            // decide on the list of parameters depending on whether a module containing
            // the init function has a struct thats an one-time-witness candidate struct
            let otw_candidate = Symbol::from(mod_ident.module.value().to_uppercase());
            let init_snippet = if mdef.structs().contains_key(&otw_candidate) {
                format!("{INIT_FN_NAME}(${{1:witness}}: {otw_candidate}, {sui_ctx_arg}) {{\n\t${{2:}}\n}}\n")
            } else {
                format!("{INIT_FN_NAME}({sui_ctx_arg}) {{\n\t${{1:}}\n}}\n")
            };

            let init_completion = CompletionItem {
                label: INIT_FN_NAME.to_string(),
                kind: Some(CompletionItemKind::SNIPPET),
                documentation: Some(Documentation::String(
                    "Module initializer snippet".to_string(),
                )),
                insert_text: Some(init_snippet),
                insert_text_format: Some(InsertTextFormat::SNIPPET),
                ..Default::default()
            };
            completions.push(init_completion);
            break;
        }
    }
    (completions, only_custom_items)
}

/// Checks if a use at a given position is also a definition.
fn is_definition(
    symbols: &Symbols,
    use_line: u32,
    use_col: u32,
    use_fhash: FileHash,
    def_loc: Loc,
) -> bool {
    if let Some(use_loc) = symbols
        .files
        .line_char_offset_to_loc_opt(use_fhash, use_line, use_col)
    {
        // TODO: is overlapping better?
        def_loc.contains(&use_loc)
    } else {
        false
    }
}

/// Finds white-space separated strings on the line containing auto-completion request and their
/// locations.
fn preceding_strings(buffer: &str, position: &Position) -> Vec<(String, u32)> {
    let mut strings = vec![];
    let line = match buffer.lines().nth(position.line as usize) {
        Some(line) => line,
        None => return strings, // Our buffer does not contain the line, and so must be out of date.
    };

    let mut chars = line.chars();
    let mut cur_col = 0;
    let mut cur_str_start = 0;
    let mut cur_str = "".to_string();
    while cur_col <= position.character {
        let Some(c) = chars.next() else {
            return strings;
        };
        if c == ' ' || c == '\t' {
            if !cur_str.is_empty() {
                // finish an already started string
                strings.push((cur_str, cur_str_start));
                cur_str = "".to_string();
            }
        } else {
            if cur_str.is_empty() {
                // start a new string
                cur_str_start = cur_col;
            }
            cur_str.push(c);
        }

        cur_col += c.len_utf8() as u32;
    }
    if !cur_str.is_empty() {
        // finish the last string
        strings.push((cur_str, cur_str_start));
    }
    strings
}

/// Sends the given connection a response to a completion request.
///
/// The completions returned depend upon where the user's cursor is positioned.
pub fn on_completion_request(
    context: &Context,
    request: &Request,
    ide_files_root: VfsPath,
    pkg_dependencies: Arc<Mutex<BTreeMap<PathBuf, PrecompiledPkgDeps>>>,
) {
    eprintln!("handling completion request");
    let parameters = serde_json::from_value::<CompletionParams>(request.params.clone())
        .expect("could not deserialize completion request");

    let path = parameters
        .text_document_position
        .text_document
        .uri
        .to_file_path()
        .unwrap();

    let mut pos = parameters.text_document_position.position;
    if pos.character != 0 {
        // adjust column to be at the character that has just been inserted rather than right after
        // it (unless we are at the very first column)
        pos = Position::new(pos.line, pos.character - 1);
    }
    let items = completions_with_context(context, ide_files_root, pkg_dependencies, &path, pos)
        .unwrap_or_default();

    let result = serde_json::to_value(items).expect("could not serialize completion response");
    eprintln!("about to send completion response");
    let response = lsp_server::Response::new_ok(request.id.clone(), result);
    if let Err(err) = context
        .connection
        .sender
        .send(lsp_server::Message::Response(response))
    {
        eprintln!("could not send completion response: {:?}", err);
    }
}

pub fn completions_with_context(
    context: &Context,
    ide_files_root: VfsPath,
    pkg_dependencies: Arc<Mutex<BTreeMap<PathBuf, PrecompiledPkgDeps>>>,
    path: &Path,
    pos: Position,
) -> Option<Vec<CompletionItem>> {
    let Some(pkg_path) = SymbolicatorRunner::root_dir(path) else {
        eprintln!("failed completion for {:?} (package root not found)", path);
        return None;
    };
    let symbol_map = context.symbols.lock().unwrap();
    let current_symbols = symbol_map.get(&pkg_path)?;
    Some(completion_items(
        current_symbols,
        ide_files_root,
        pkg_dependencies,
        path,
        pos,
    ))
}

pub fn completion_items(
    current_symbols: &Symbols,
    ide_files_root: VfsPath,
    pkg_dependencies: Arc<Mutex<BTreeMap<PathBuf, PrecompiledPkgDeps>>>,
    path: &Path,
    pos: Position,
) -> Vec<CompletionItem> {
    compute_cursor_completion_items(ide_files_root, pkg_dependencies, path, pos)
        .unwrap_or_else(|| compute_completion_items(current_symbols, path, pos))
}

fn compute_cursor_completion_items(
    ide_files_root: VfsPath,
    pkg_dependencies: Arc<Mutex<BTreeMap<PathBuf, PrecompiledPkgDeps>>>,
    path: &Path,
    cursor_position: Position,
) -> Option<Vec<CompletionItem>> {
    let Some(pkg_path) = SymbolicatorRunner::root_dir(path) else {
        eprintln!("failed completion for {:?} (package root not found)", path);
        return None;
    };
    let cursor_path = path.to_path_buf();
    let cursor_info = Some((&cursor_path, cursor_position));
    let (symbols, _diags) = symbols::get_symbols(
        pkg_dependencies,
        ide_files_root,
        &pkg_path,
        LintLevel::None,
        cursor_info,
    )
    .ok()?;
    let symbols = symbols?;
    Some(compute_completion_items(&symbols, path, cursor_position))
}

/// Computes completion items for a given completion request.
fn compute_completion_items(symbols: &Symbols, path: &Path, pos: Position) -> Vec<CompletionItem> {
    let mut items = vec![];

    let Some(fhash) = symbols.file_hash(path) else {
        return items;
    };
    let Some(file_id) = symbols.files.file_mapping().get(&fhash) else {
        return items;
    };
    let Ok(file) = symbols.files.files().get(*file_id) else {
        return items;
    };

    let file_source = file.source().clone();
    if !file_source.is_empty() {
        let only_custom_items;
        match &symbols.cursor_context {
            Some(cursor_context) => {
                eprintln!("cursor completion");
                let (new_items, only_has_custom_items) =
                    cursor_completion_items(symbols, path, &file_source, pos, cursor_context);
                only_custom_items = only_has_custom_items;
                items.extend(new_items);
            }
            None => {
                eprintln!("non-cursor completion");
                let (new_items, only_has_custom_items) =
                    default_items(symbols, path, &file_source, pos);
                only_custom_items = only_has_custom_items;
                items.extend(new_items);
            }
        }
        if !only_custom_items {
            let identifiers = identifiers(&file_source, symbols, path);
            items.extend(identifiers);
        }
    } else {
        // no file content
        items.extend(keywords());
        items.extend(builtins());
    }
    items
}

/// Return completion items, plus a flag indicating if we should only use the custom items returned
/// (i.e., when the flag is false, default items and identifiers should also be added).
fn cursor_completion_items(
    symbols: &Symbols,
    path: &Path,
    file_source: &str,
    pos: Position,
    cursor: &CursorContext,
) -> (Vec<CompletionItem>, bool) {
    let cursor_leader = get_cursor_token(file_source, &pos);
    match cursor_leader {
        Some(Tok::Colon) => {
            // TODO: sweep current scope and find types there.
            (primitive_types(), false)
        }
        // TODO: consider using `cursor.position` for this instead
        Some(Tok::Period) => {
            eprintln!("found period");
            let items = dot(symbols, path, &pos);
            let items_is_empty = items.is_empty();
            eprintln!("found items: {}", !items_is_empty);
            // whether completions have been found for the dot or not
            // it makes no sense to try offering "dumb" autocompletion
            // options here as they will not fit (an example would
            // be dot completion of u64 variable without any methods
            // with u64 receiver being visible)
            (items, true)
        }
        // TODO: consider using `cursor.position` for this instead
        Some(Tok::ColonColon) => {
            // `.` or `::` must be followed by identifiers, which are added to the completion items
            // below.
            // TODO: consider the cursor and see if we can handle a path there
            match &cursor.position {
                symbols::CursorPosition::Exp(sp!(_, P::Exp_::Name(name))) => {
                    match &name.value {
                        P::NameAccessChain_::Single(_name) => {
                            // This is technically unreachable because we wouldn't be at a `::`
                            (vec![], false)
                        }
                        P::NameAccessChain_::Path(path) => {
                            let P::NamePath { root, entries } = path;
                            if root.name.loc.contains(&cursor.loc) {
                                // This is technically unreachable because we wouldn't be at a `::`
                                (vec![], false)
                            } else {
                                for entry in entries {
                                    if entry.name.loc.contains(&cursor.loc) {
                                        // TODO: figure out what the name parts refers to, look it
                                        // up in typing, and go from there.
                                        return (vec![], false);
                                    }
                                }
                                (vec![], false)
                            }
                        }
                    }
                }
                _ => (vec![], false),
            }
        }
        // Carve out to suggest UID for struct with key ability
        Some(Tok::LBrace) => (
            context_specific_lbrace(symbols, cursor).unwrap_or_default(),
            true,
        ),
        _ => {
            eprintln!("no relevant cursor leader");
            let mut items = vec![];
            let mut only_custom_items = false;
            if let symbols::CursorPosition::Exp(sp!(_, P::Exp_::Name(_name))) = &cursor.position {
                // TODO: match on the name and use provided compiler info to resolve this
                // (see PR 18108 for more details on that information)
            }
            eprintln!("checking default items");
            let (default_items, default_custom) = default_items(symbols, path, file_source, pos);
            items.extend(default_items);
            only_custom_items |= default_custom;
            (items, only_custom_items)
        }
    }
}

fn default_items(
    symbols: &Symbols,
    path: &Path,
    file_source: &str,
    pos: Position,
) -> (Vec<CompletionItem>, bool) {
    // If the user's cursor is positioned anywhere other than following a `.`, `:`, or `::`,
    // offer them context-specific autocompletion items as well as
    // Move's keywords, operators, and builtins.
    let (custom_items, only_custom_items) =
        context_specific_no_trigger(symbols, path, file_source, &pos);
    let mut items = custom_items;
    if !only_custom_items {
        items.extend(keywords());
        items.extend(builtins());
    }
    (items, only_custom_items)
}
