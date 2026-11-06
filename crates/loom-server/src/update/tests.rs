use std::{fs, path::PathBuf};

use super::*;
use loom_release::{ReleaseAsset, ReleaseInfo};

#[test]
fn an_install_layout_is_recognized_from_the_executable_path() {
    let exe = Path::new("/state/loom/server/v1.2.3/loom-server");
    let found = InstallLayout::from_exe(exe).expect("a managed install");
    assert_eq!(found.state, PathBuf::from("/state/loom"));
    assert_eq!(found.tag, "v1.2.3");
    assert_eq!(
        found.binary("v2.0.0"),
        PathBuf::from("/state/loom/server/v2.0.0/loom-server")
    );

    // A development build, a system path, and a different directory are not
    // managed installs, so they never replace themselves.
    assert_eq!(
        InstallLayout::from_exe(Path::new("/state/loom/server/0.1.0/loom-server")),
        None
    );
    assert_eq!(
        InstallLayout::from_exe(Path::new("/usr/bin/loom-server")),
        None
    );
    assert_eq!(
        InstallLayout::from_exe(Path::new("/state/loom/build/v1.2.3/loom-server")),
        None
    );
}

#[test]
fn a_unit_is_pointed_at_the_new_tag() {
    let absolute = "[Service]\n\
                    ExecStart=\"/state/loom/server/v1.0.0/loom-server\" --bind 127.0.0.1:8765\n";
    let updated = rewritten_unit_text(absolute, "v1.0.0", "v1.1.0").expect("the unit matches");
    assert!(updated.contains("/server/v1.1.0/loom-server"), "{updated}");
    assert!(!updated.contains("v1.0.0"), "{updated}");

    // The SSH provisioner uses the `%h` specifier rather than an absolute path.
    let ssh = "ExecStart=%h/.loom/server/v1.0.0/loom-server --bind 127.0.0.1:8765\n";
    let updated = rewritten_unit_text(ssh, "v1.0.0", "v1.1.0").expect("the unit matches");
    assert!(
        updated.contains("%h/.loom/server/v1.1.0/loom-server"),
        "{updated}"
    );

    assert_eq!(
        rewritten_unit_text("ExecStart=/elsewhere/loom-server", "v1.0.0", "v1.1.0"),
        None
    );
}

struct Fixture {
    release: ReleaseInfo,
}

impl ReleaseSource for Fixture {
    fn release(&self, tag: Option<&str>) -> Result<Option<ReleaseInfo>> {
        Ok(tag
            .is_none_or(|tag| tag == self.release.tag_name)
            .then(|| self.release.clone()))
    }

    fn download(&self, _: &str) -> Result<Vec<u8>> {
        Err(LoomError::new(
            ErrorCode::NotFound,
            "the decision test never downloads".to_owned(),
            false,
        ))
    }
}

fn fixture(tag: &str) -> Fixture {
    Fixture {
        release: ReleaseInfo {
            tag_name: tag.to_owned(),
            assets: Vec::<ReleaseAsset>::new(),
        },
    }
}

#[test]
fn only_a_newer_stable_release_is_installed() {
    let root = std::env::temp_dir().join(format!("loom-update-{}", uuid::Uuid::new_v4()));
    let layout = InstallLayout {
        state: root.clone(),
        tag: "v1.0.0".to_owned(),
    };

    // The same release, an older one, and a development build are not updates.
    assert_eq!(
        check_and_install(&fixture("v1.0.0"), &layout, "v1.0.0").unwrap(),
        None
    );
    assert_eq!(
        check_and_install(&fixture("v0.9.0"), &layout, "v1.0.0").unwrap(),
        None
    );
    assert_eq!(
        check_and_install(&fixture("v2.0.0"), &layout, "0.1.0").unwrap(),
        None
    );

    // A newer release that is already installed is reported without a download.
    let target = layout.binary("v1.1.0");
    fs::create_dir_all(target.parent().unwrap()).unwrap();
    fs::write(&target, b"installed").unwrap();
    assert_eq!(
        check_and_install(&fixture("v1.1.0"), &layout, "v1.0.0").unwrap(),
        Some("v1.1.0".to_owned())
    );

    fs::remove_dir_all(root).unwrap();
}
