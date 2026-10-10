//! The wasm asset bundle.
//!
//! The browser build embeds only the icons the client can render. Native
//! builds register the full gpui-kit catalog instead, so this list must cover
//! both Loom's own icons and the icons the gpui-component widgets render
//! internally (selects, menus, dialogs, inputs, lists, and buttons).
gpui_kit::assets::icon_assets!(
    pub(crate) LoomAssets,
    [
        // Loom controls and timeline.
        Archive,
        ArrowDown,
        ArrowUp,
        BotMessageSquare,
        Brain,
        Check,
        ChevronLeft,
        ChevronUp,
        Circle,
        Clock,
        Close,
        Command,
        Copy,
        Ellipsis,
        ExternalLink,
        FileText,
        Folder,
        FolderOpen,
        GitBranch,
        GitMerge,
        Globe,
        List,
        ListChecks,
        ListTree,
        LoaderCircle,
        MessageSquare,
        MessagesSquare,
        PanelRight,
        PanelRightClose,
        PanelRightOpen,
        Pencil,
        Play,
        Plus,
        Settings,
        Sparkles,
        SquareTerminal,
        User,
        UserPlus,
        Users,
        Workflow,
        Wrench,
        // Rendered by gpui-component widgets Loom uses.
        ChevronDown,
        ChevronRight,
        Eye,
        EyeOff,
        Inbox,
        Loader,
        Search,
    ]
);

#[cfg(test)]
mod tests {
    use super::LoomAssets;
    use gpui_kit::AssetSource as _;
    use gpui_kit::assets::IconName;
    use std::collections::{BTreeMap, BTreeSet};
    use std::fs;
    use std::path::Path;

    /// Icons rendered internally by gpui-component widgets Loom uses but never
    /// named in Loom's own source: Select and Input (chevron down/right, eye,
    /// eye off), the searchable list (search), the popup menu (chevron right),
    /// the select empty state (inbox), and the button loading state (loader).
    ///
    /// Keep this in step when the widgets Loom uses change; a widget that starts
    /// rendering a new internal icon will not be caught by the source scan.
    const COMPONENT_ICONS: [&str; 7] = [
        "ChevronDown",
        "ChevronRight",
        "Eye",
        "EyeOff",
        "Inbox",
        "Loader",
        "Search",
    ];

    /// Collects every Rust source file under `root`, recursively.
    fn source_files(root: &Path, sources: &mut Vec<String>) {
        for entry in fs::read_dir(root).expect("read client source directory") {
            let path = entry.expect("read client source entry").path();
            if path.is_dir() {
                source_files(&path, sources);
            } else if path.extension().and_then(|ext| ext.to_str()) == Some("rs") {
                sources.push(fs::read_to_string(&path).expect("read client source"));
            }
        }
    }

    /// Every icon the client can render must resolve in the embedded bundle.
    ///
    /// The browser build has no fallback asset source, so a missing entry here
    /// silently renders a blank icon. Every `.rs` file in the crate is scanned
    /// for `IconName::`/`AssetIconName::` references so an icon added anywhere,
    /// not just in `view.rs`, fails this test until it is embedded.
    #[test]
    fn embeds_every_icon_the_client_can_render() {
        let assets = LoomAssets;

        let mut sources = Vec::new();
        source_files(
            &Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
            &mut sources,
        );

        let mut referenced = BTreeSet::new();
        for source in &sources {
            for prefix in ["IconName::", "AssetIconName::"] {
                let mut rest = source.as_str();
                while let Some(index) = rest.find(prefix) {
                    let after = &rest[index + prefix.len()..];
                    let name = after
                        .chars()
                        .take_while(|character| character.is_ascii_alphanumeric())
                        .collect::<String>();
                    // `IconName::ALL` is the catalog iterator, not an icon.
                    if !name.is_empty() && name != "ALL" {
                        referenced.insert(name);
                    }
                    rest = after;
                }
            }
        }
        for name in COMPONENT_ICONS {
            referenced.insert(name.to_owned());
        }

        let mut by_name = BTreeMap::new();
        for icon in IconName::ALL {
            by_name.insert(format!("{icon:?}"), *icon);
        }
        for name in &referenced {
            let Some(icon) = by_name.get(name) else {
                panic!("the client references unknown icon {name}");
            };
            let path = icon.path();
            assert!(
                assets.load(path.as_ref()).unwrap().is_some(),
                "icon {path} is not embedded in the wasm asset bundle"
            );
        }
    }
}
