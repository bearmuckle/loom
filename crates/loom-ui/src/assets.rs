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
        ArrowDown,
        Brain,
        Check,
        Clock,
        Close,
        Command,
        Copy,
        Ellipsis,
        ExternalLink,
        FileText,
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

    /// Every icon the client can render must resolve in the embedded bundle.
    ///
    /// The browser build has no fallback asset source, so a missing entry here
    /// silently renders a blank icon. Icons referenced directly in the view are
    /// scanned from its source; the component-internal icons are listed
    /// explicitly and should be updated when the widgets Loom uses change.
    #[test]
    fn embeds_every_icon_the_client_can_render() {
        let assets = LoomAssets;

        let mut referenced = BTreeSet::new();
        let source = include_str!("view.rs");
        for prefix in ["IconName::", "AssetIconName::"] {
            let mut rest = source;
            while let Some(index) = rest.find(prefix) {
                let after = &rest[index + prefix.len()..];
                let name = after
                    .chars()
                    .take_while(|character| character.is_ascii_alphanumeric())
                    .collect::<String>();
                if !name.is_empty() {
                    referenced.insert(name);
                }
                rest = after;
            }
        }

        // Rendered internally by Select, PopupMenu, Dialog, Input, the list,
        // and Button.
        for name in [
            "ChevronDown",
            "ChevronRight",
            "Eye",
            "EyeOff",
            "Inbox",
            "Loader",
            "Search",
        ] {
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
