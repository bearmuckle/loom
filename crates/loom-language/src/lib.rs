use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

use loom_core::{ErrorCode, LoomError, Result};
use loom_workspace::Workspace;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct LanguageId(String);

impl LanguageId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into().to_ascii_lowercase())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn for_path(path: &str) -> Self {
        let extension = path.rsplit('.').next().unwrap_or_default();
        Self::new(match extension {
            "rs" => "rust",
            "md" | "markdown" => "markdown",
            "json" => "json",
            "toml" => "toml",
            "js" | "jsx" => "javascript",
            "ts" | "tsx" => "typescript",
            "py" => "python",
            _ => "plaintext",
        })
    }
}

impl From<&str> for LanguageId {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LanguageServiceState {
    Stopped,
    Starting,
    Ready,
    Unavailable,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct LanguageServiceCapabilities {
    pub diagnostics: bool,
    pub symbols: bool,
    pub go_to_definition: bool,
    pub references: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Ord, PartialOrd, Serialize)]
pub struct Position {
    pub line: u32,
    pub character: u32,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Range {
    pub start: Position,
    pub end: Position,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Location {
    pub path: String,
    pub range: Range,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticSeverity {
    Error,
    Warning,
    Information,
    Hint,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Diagnostic {
    pub range: Range,
    pub severity: DiagnosticSeverity,
    pub message: String,
    pub source: String,
    pub code: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SymbolKind {
    File,
    Module,
    Function,
    Struct,
    Enum,
    Trait,
    Class,
    Method,
    Constant,
    Variable,
    Property,
    Heading,
    Key,
    Other,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Symbol {
    pub name: String,
    pub kind: SymbolKind,
    pub location: Location,
    pub container: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct LanguageServiceDescriptor {
    pub language_id: LanguageId,
    pub state: LanguageServiceState,
    pub capabilities: LanguageServiceCapabilities,
    pub implementation: String,
    pub error: Option<String>,
}

pub trait LanguageService: Send + Sync {
    fn descriptor(&self) -> LanguageServiceDescriptor;
    fn start(&self) -> Result<()>;
    fn stop(&self) -> Result<()>;
    fn diagnostics(&self, path: &str, text: &str) -> Result<Vec<Diagnostic>>;
    fn symbols(&self, path: &str, text: &str) -> Result<Vec<Symbol>>;
}

#[derive(Clone, Debug)]
pub struct BasicLanguageService {
    language_id: LanguageId,
    state: Arc<Mutex<LanguageServiceState>>,
}

impl BasicLanguageService {
    pub fn new(language_id: impl Into<LanguageId>) -> Self {
        Self {
            language_id: language_id.into(),
            state: Arc::new(Mutex::new(LanguageServiceState::Stopped)),
        }
    }

    pub fn language_id(&self) -> &LanguageId {
        &self.language_id
    }

    pub fn capabilities_for(language_id: &LanguageId) -> LanguageServiceCapabilities {
        let supported = matches!(
            language_id.as_str(),
            "rust" | "markdown" | "json" | "toml" | "javascript" | "typescript" | "python"
        );
        LanguageServiceCapabilities {
            diagnostics: supported,
            symbols: supported,
            go_to_definition: supported,
            references: supported,
        }
    }

    fn state(&self) -> Result<LanguageServiceState> {
        self.state.lock().map(|state| *state).map_err(|_| {
            LoomError::new(
                ErrorCode::Internal,
                "language service state lock was poisoned",
                true,
            )
        })
    }

    fn ensure_ready(&self, capability: &str) -> Result<()> {
        let state = self.state()?;
        if state != LanguageServiceState::Ready {
            return Err(LoomError::new(
                ErrorCode::LanguageService,
                format!(
                    "basic {} language service is {state:?}; start it before {capability}",
                    self.language_id.as_str()
                ),
                false,
            ));
        }
        if !capability_supported(&Self::capabilities_for(&self.language_id), capability) {
            return Err(LoomError::new(
                ErrorCode::UnsupportedCapability,
                format!(
                    "language service '{}' does not support {capability}",
                    self.language_id.as_str()
                ),
                false,
            ));
        }
        Ok(())
    }
}

impl LanguageService for BasicLanguageService {
    fn descriptor(&self) -> LanguageServiceDescriptor {
        let state = self.state().unwrap_or(LanguageServiceState::Unavailable);
        LanguageServiceDescriptor {
            language_id: self.language_id.clone(),
            state,
            capabilities: Self::capabilities_for(&self.language_id),
            implementation: "loom-basic".to_owned(),
            error: (state == LanguageServiceState::Unavailable)
                .then(|| "language service is unavailable".to_owned()),
        }
    }

    fn start(&self) -> Result<()> {
        let mut state = self.state.lock().map_err(|_| {
            LoomError::new(
                ErrorCode::Internal,
                "language service state lock was poisoned",
                true,
            )
        })?;
        if *state == LanguageServiceState::Unavailable {
            return Err(LoomError::new(
                ErrorCode::LanguageService,
                format!(
                    "language service '{}' is unavailable",
                    self.language_id.as_str()
                ),
                false,
            ));
        }
        *state = LanguageServiceState::Starting;
        *state = LanguageServiceState::Ready;
        Ok(())
    }

    fn stop(&self) -> Result<()> {
        let mut state = self.state.lock().map_err(|_| {
            LoomError::new(
                ErrorCode::Internal,
                "language service state lock was poisoned",
                true,
            )
        })?;
        *state = LanguageServiceState::Stopped;
        Ok(())
    }

    fn diagnostics(&self, path: &str, text: &str) -> Result<Vec<Diagnostic>> {
        self.ensure_ready("diagnostics")?;
        Ok(basic_diagnostics(path, text))
    }

    fn symbols(&self, path: &str, text: &str) -> Result<Vec<Symbol>> {
        self.ensure_ready("symbols")?;
        Ok(basic_symbols(&self.language_id, path, text))
    }
}

#[derive(Clone, Debug)]
pub struct LanguageServiceManager {
    services: BTreeMap<LanguageId, BasicLanguageService>,
}

impl Default for LanguageServiceManager {
    fn default() -> Self {
        Self::basic()
    }
}

impl LanguageServiceManager {
    pub fn basic() -> Self {
        let mut services = BTreeMap::new();
        for language in [
            "rust",
            "markdown",
            "json",
            "toml",
            "javascript",
            "typescript",
            "python",
        ] {
            services.insert(
                LanguageId::new(language),
                BasicLanguageService::new(language),
            );
        }
        Self { services }
    }

    pub fn register_basic(&mut self, language_id: impl Into<LanguageId>) {
        let language_id = language_id.into();
        self.services
            .insert(language_id.clone(), BasicLanguageService::new(language_id));
    }

    pub fn descriptors(&self) -> Vec<LanguageServiceDescriptor> {
        self.services
            .values()
            .map(|service| service.descriptor())
            .collect()
    }

    pub fn descriptor_for_path(&self, path: &str) -> LanguageServiceDescriptor {
        let language_id = LanguageId::for_path(path);
        self.services
            .get(&language_id)
            .map(LanguageService::descriptor)
            .unwrap_or(LanguageServiceDescriptor {
                language_id,
                state: LanguageServiceState::Unavailable,
                capabilities: LanguageServiceCapabilities::default(),
                implementation: "none".to_owned(),
                error: Some("no language service is registered for this file".to_owned()),
            })
    }

    pub fn start_for_path(&self, path: &str) -> Result<LanguageServiceDescriptor> {
        let service = self.service_for_path(path)?;
        service.start()?;
        Ok(service.descriptor())
    }

    pub fn stop_for_path(&self, path: &str) -> Result<LanguageServiceDescriptor> {
        let service = self.service_for_path(path)?;
        service.stop()?;
        Ok(service.descriptor())
    }

    pub fn diagnostics(&self, workspace: &Workspace, path: &str) -> Result<Vec<Diagnostic>> {
        let service = self.service_for_path(path)?;
        let file = workspace.read_file(path)?;
        service.diagnostics(path, &file.content)
    }

    pub fn symbols(&self, workspace: &Workspace, path: &str) -> Result<Vec<Symbol>> {
        let service = self.service_for_path(path)?;
        let file = workspace.read_file(path)?;
        service.symbols(path, &file.content)
    }

    pub fn go_to_definition(
        &self,
        workspace: &Workspace,
        path: &str,
        position: Position,
    ) -> Result<Option<Location>> {
        let service = self.service_for_path(path)?;
        service.ensure_ready("go_to_definition")?;
        let file = workspace.read_file(path)?;
        let word = word_at(&file.content, position)?;
        for entry in workspace.snapshot()?.entries {
            if entry.kind != loom_workspace::WorkspaceEntryKind::File {
                continue;
            }
            let candidate = match workspace.read_file(&entry.path) {
                Ok(candidate) => candidate,
                Err(_) => continue,
            };
            let language = LanguageId::for_path(&entry.path);
            let Some(service) = self.services.get(&language) else {
                continue;
            };
            if service.state()? != LanguageServiceState::Ready {
                continue;
            }
            if let Some(symbol) = basic_symbols(&language, &entry.path, &candidate.content)
                .into_iter()
                .find(|symbol| symbol.name == word)
            {
                return Ok(Some(symbol.location));
            }
        }
        Ok(None)
    }

    pub fn references(
        &self,
        workspace: &Workspace,
        path: &str,
        position: Position,
    ) -> Result<Vec<Location>> {
        let service = self.service_for_path(path)?;
        service.ensure_ready("references")?;
        let source = workspace.read_file(path)?;
        let word = word_at(&source.content, position)?;
        let mut locations = Vec::new();
        for entry in workspace.snapshot()?.entries {
            if entry.kind != loom_workspace::WorkspaceEntryKind::File {
                continue;
            }
            let candidate = match workspace.read_file(&entry.path) {
                Ok(candidate) => candidate,
                Err(_) => continue,
            };
            for (line_index, line) in candidate.content.lines().enumerate() {
                let mut offset = 0;
                while let Some(found) = find_identifier(&line[offset..], &word) {
                    let start = offset + found;
                    locations.push(Location {
                        path: entry.path.clone(),
                        range: Range {
                            start: Position {
                                line: line_index as u32,
                                character: start as u32,
                            },
                            end: Position {
                                line: line_index as u32,
                                character: (start + word.len()) as u32,
                            },
                        },
                    });
                    offset = start.saturating_add(word.len().max(1));
                    if offset >= line.len() {
                        break;
                    }
                }
            }
        }
        Ok(locations)
    }

    fn service_for_path(&self, path: &str) -> Result<&BasicLanguageService> {
        self.services
            .get(&LanguageId::for_path(path))
            .ok_or_else(|| {
                LoomError::new(
                    ErrorCode::UnsupportedCapability,
                    format!(
                        "no language service is registered for '{}'",
                        LanguageId::for_path(path).as_str()
                    ),
                    false,
                )
            })
    }
}

fn capability_supported(capabilities: &LanguageServiceCapabilities, name: &str) -> bool {
    match name {
        "diagnostics" => capabilities.diagnostics,
        "symbols" => capabilities.symbols,
        "go_to_definition" => capabilities.go_to_definition,
        "references" => capabilities.references,
        _ => false,
    }
}

fn basic_diagnostics(path: &str, text: &str) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();
    let mut stack = Vec::new();
    for (line_index, line) in text.lines().enumerate() {
        if let Some(column) = line.find("TODO") {
            diagnostics.push(Diagnostic {
                range: line_range(line_index, column, column + 4),
                severity: DiagnosticSeverity::Hint,
                message: "TODO item is not resolved".to_owned(),
                source: "loom-basic".to_owned(),
                code: Some("todo".to_owned()),
            });
        }
        if let Some(column) = line.find("ERROR") {
            diagnostics.push(Diagnostic {
                range: line_range(line_index, column, column + 5),
                severity: DiagnosticSeverity::Error,
                message: "explicit ERROR marker".to_owned(),
                source: "loom-basic".to_owned(),
                code: Some("error-marker".to_owned()),
            });
        }
        for (column, character) in line.char_indices() {
            if "([{".contains(character) {
                stack.push((character, line_index as u32, column as u32));
            } else if ")]}".contains(character) {
                let matches = stack
                    .last()
                    .is_some_and(|(open, _, _)| matching_delimiter(*open, character));
                if matches {
                    stack.pop();
                } else {
                    diagnostics.push(Diagnostic {
                        range: line_range(line_index, column, column + character.len_utf8()),
                        severity: DiagnosticSeverity::Error,
                        message: format!("unmatched delimiter '{character}' in {path}"),
                        source: "loom-basic".to_owned(),
                        code: Some("unmatched-delimiter".to_owned()),
                    });
                }
            }
        }
    }
    for (open, line, column) in stack {
        diagnostics.push(Diagnostic {
            range: line_range(line as usize, column as usize, column as usize + 1),
            severity: DiagnosticSeverity::Error,
            message: format!("unclosed delimiter '{open}'"),
            source: "loom-basic".to_owned(),
            code: Some("unclosed-delimiter".to_owned()),
        });
    }
    diagnostics
}

fn basic_symbols(language: &LanguageId, path: &str, text: &str) -> Vec<Symbol> {
    let mut symbols = Vec::new();
    for (line_index, line) in text.lines().enumerate() {
        let mut trimmed = line.trim_start();
        let mut leading = line.len().saturating_sub(trimmed.len());
        while let Some(prefix) = ["pub ", "async ", "unsafe "]
            .iter()
            .find(|prefix| trimmed.starts_with(**prefix))
        {
            leading += prefix.len();
            trimmed = &trimmed[prefix.len()..];
        }
        let (kind, name, offset) = if language.as_str() == "markdown" {
            let hashes = trimmed
                .chars()
                .take_while(|character| *character == '#')
                .count();
            if hashes > 0 && trimmed.chars().nth(hashes) == Some(' ') {
                (
                    SymbolKind::Heading,
                    trimmed[hashes + 1..].trim().to_owned(),
                    leading + hashes + 1,
                )
            } else {
                continue;
            }
        } else if language.as_str() == "json" {
            let Some((name, position)) = json_key(trimmed) else {
                continue;
            };
            (SymbolKind::Key, name, leading + position)
        } else {
            let patterns = [
                ("fn ", SymbolKind::Function),
                ("struct ", SymbolKind::Struct),
                ("enum ", SymbolKind::Enum),
                ("trait ", SymbolKind::Trait),
                ("mod ", SymbolKind::Module),
                ("class ", SymbolKind::Class),
                ("def ", SymbolKind::Function),
                ("const ", SymbolKind::Constant),
                ("type ", SymbolKind::Other),
            ];
            let Some((prefix, kind)) = patterns
                .iter()
                .find(|(prefix, _)| trimmed.starts_with(prefix))
            else {
                continue;
            };
            let rest = &trimmed[prefix.len()..];
            let name = identifier_prefix(rest);
            if name.is_empty() {
                continue;
            }
            (*kind, name.to_owned(), leading + prefix.len())
        };
        let end = offset + name.len();
        symbols.push(Symbol {
            name,
            kind,
            location: Location {
                path: path.to_owned(),
                range: line_range(line_index, offset, end),
            },
            container: None,
        });
    }
    symbols
}

fn word_at(text: &str, position: Position) -> Result<String> {
    let line = text
        .lines()
        .nth(position.line as usize)
        .ok_or_else(|| LoomError::invalid_request("language position is outside the document"))?;
    let character = position.character as usize;
    if character > line.len() {
        return Err(LoomError::invalid_request(
            "language position is outside the document",
        ));
    }
    let mut start = character;
    while start > 0 && is_identifier_byte(line.as_bytes()[start - 1]) {
        start -= 1;
    }
    let mut end = character;
    while end < line.len() && is_identifier_byte(line.as_bytes()[end]) {
        end += 1;
    }
    if start == end {
        return Err(LoomError::invalid_request(
            "language position is not on an identifier",
        ));
    }
    Ok(line[start..end].to_owned())
}

fn find_identifier(text: &str, needle: &str) -> Option<usize> {
    let mut offset = 0;
    while let Some(found) = text[offset..].find(needle) {
        let start = offset + found;
        let end = start + needle.len();
        let before = start == 0 || !is_identifier_byte(text.as_bytes()[start - 1]);
        let after = end == text.len() || !is_identifier_byte(text.as_bytes()[end]);
        if before && after {
            return Some(start);
        }
        offset = end;
        if offset >= text.len() {
            return None;
        }
    }
    None
}

fn identifier_prefix(text: &str) -> &str {
    let end = text
        .char_indices()
        .find(|(_, character)| !is_identifier_byte(*character as u8))
        .map_or(text.len(), |(index, _)| index);
    &text[..end]
}

fn json_key(text: &str) -> Option<(String, usize)> {
    let text = text.trim_start();
    if !text.starts_with('"') {
        return None;
    }
    let end = text[1..].find('"')? + 1;
    Some((text[1..end].to_owned(), 1))
}

fn is_identifier_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

fn line_range(line: usize, start: usize, end: usize) -> Range {
    Range {
        start: Position {
            line: line as u32,
            character: start as u32,
        },
        end: Position {
            line: line as u32,
            character: end as u32,
        },
    }
}

fn matching_delimiter(open: char, close: char) -> bool {
    matches!((open, close), ('(', ')') | ('[', ']') | ('{', '}'))
}

#[cfg(test)]
mod tests {
    use std::{fs, path::PathBuf};

    use loom_core::ProjectId;

    use super::*;

    fn workspace() -> (Workspace, PathBuf) {
        let root = std::env::temp_dir().join(format!("loom-language-{}", ProjectId::new()));
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), "pub fn answer() {\n    TODO\n}\n").unwrap();
        fs::write(root.join("src/main.rs"), "fn main() { answer(); }\n").unwrap();
        (Workspace::open(ProjectId::new(), &root).unwrap(), root)
    }

    #[test]
    fn basic_service_exposes_lifecycle_capabilities_and_diagnostics() {
        let service = BasicLanguageService::new("rust");
        assert_eq!(service.descriptor().state, LanguageServiceState::Stopped);
        service.start().unwrap();
        assert!(service.descriptor().capabilities.diagnostics);
        let diagnostics = service
            .diagnostics("src/lib.rs", "fn main() {\n TODO\n")
            .unwrap();
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code.as_deref() == Some("todo"))
        );
    }

    #[test]
    fn manager_finds_symbols_definitions_and_references() {
        let (workspace, root) = workspace();
        let manager = LanguageServiceManager::basic();
        manager.start_for_path("src/lib.rs").unwrap();
        manager.start_for_path("src/main.rs").unwrap();
        let symbols = manager.symbols(&workspace, "src/lib.rs").unwrap();
        assert!(symbols.iter().any(|symbol| symbol.name == "answer"));
        let definition = manager
            .go_to_definition(
                &workspace,
                "src/main.rs",
                Position {
                    line: 0,
                    character: 13,
                },
            )
            .unwrap();
        assert_eq!(definition.unwrap().path, "src/lib.rs");
        let references = manager
            .references(
                &workspace,
                "src/lib.rs",
                Position {
                    line: 0,
                    character: 8,
                },
            )
            .unwrap();
        assert!(references.len() >= 2);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn unavailable_language_is_reported_instead_of_assumed() {
        let manager = LanguageServiceManager::basic();
        let descriptor = manager.descriptor_for_path("src/example.xyz");
        assert_eq!(descriptor.state, LanguageServiceState::Unavailable);
        assert!(descriptor.error.is_some());
    }
}
