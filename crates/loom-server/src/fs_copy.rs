use super::*;

pub(crate) fn copy_directory_contents(source: &Path, destination: &Path) -> Result<()> {
    let source = fs::canonicalize(source).map_err(|error| {
        LoomError::new(
            ErrorCode::WorkspaceAccessDenied,
            format!("could not resolve local directory: {error}"),
            false,
        )
    })?;
    if !source.is_dir() {
        return Err(LoomError::invalid_request(
            "local import source must be a directory",
        ));
    }
    let destination_parent = destination
        .parent()
        .ok_or_else(|| LoomError::invalid_request("local import destination must have a parent"))?;
    fs::create_dir_all(destination_parent).map_err(|error| {
        LoomError::new(
            ErrorCode::WorkspaceAccessDenied,
            format!("could not create local import destination: {error}"),
            false,
        )
    })?;
    let destination_parent = fs::canonicalize(destination_parent).map_err(|error| {
        LoomError::new(
            ErrorCode::WorkspaceAccessDenied,
            format!("could not resolve local import destination: {error}"),
            false,
        )
    })?;
    if destination_parent.starts_with(&source) {
        return Err(LoomError::invalid_request(
            "cannot import a directory that contains the session filesystem",
        ));
    }
    let destination = destination_parent.join(
        destination
            .file_name()
            .ok_or_else(|| LoomError::invalid_request("invalid import destination"))?,
    );
    if destination.exists() {
        return Err(LoomError::conflict(
            "local import destination already exists",
        ));
    }
    let temporary = destination.with_file_name(format!(".loom-import-{}", uuid::Uuid::new_v4()));
    fs::create_dir(&temporary).map_err(|error| {
        LoomError::new(
            ErrorCode::WorkspaceAccessDenied,
            format!("could not create temporary import directory: {error}"),
            false,
        )
    })?;

    fn copy_tree(
        root: &Path,
        source: &Path,
        destination: &Path,
        ancestors: &mut BTreeSet<PathBuf>,
    ) -> Result<()> {
        let source = fs::canonicalize(source).map_err(|error| {
            LoomError::new(
                ErrorCode::WorkspaceAccessDenied,
                format!("could not resolve imported path: {error}"),
                false,
            )
        })?;
        if !source.starts_with(root) {
            return Err(LoomError::new(
                ErrorCode::WorkspaceAccessDenied,
                "local import contains a symbolic link outside its source directory",
                false,
            ));
        }
        let metadata = fs::metadata(&source).map_err(|error| {
            LoomError::new(
                ErrorCode::WorkspaceAccessDenied,
                format!("could not inspect imported path: {error}"),
                false,
            )
        })?;
        if metadata.is_dir() {
            if !ancestors.insert(source.clone()) {
                return Err(LoomError::invalid_request(
                    "local import contains a directory link cycle",
                ));
            }
            fs::create_dir_all(destination).map_err(|error| {
                LoomError::new(
                    ErrorCode::WorkspaceAccessDenied,
                    format!("could not create imported directory: {error}"),
                    false,
                )
            })?;
            for entry in fs::read_dir(&source).map_err(|error| {
                LoomError::new(
                    ErrorCode::WorkspaceAccessDenied,
                    format!("could not read imported directory: {error}"),
                    false,
                )
            })? {
                let entry = entry.map_err(|error| {
                    LoomError::new(
                        ErrorCode::WorkspaceAccessDenied,
                        format!("could not read imported entry: {error}"),
                        false,
                    )
                })?;
                copy_tree(
                    root,
                    &entry.path(),
                    &destination.join(entry.file_name()),
                    ancestors,
                )?;
            }
            ancestors.remove(&source);
        } else if metadata.is_file() {
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent).map_err(|error| {
                    LoomError::new(
                        ErrorCode::WorkspaceAccessDenied,
                        format!("could not create imported file parent: {error}"),
                        false,
                    )
                })?;
            }
            fs::copy(source, destination).map_err(|error| {
                LoomError::new(
                    ErrorCode::WorkspaceAccessDenied,
                    format!("could not copy imported file: {error}"),
                    false,
                )
            })?;
        } else {
            return Err(LoomError::invalid_request(
                "local import contains an unsupported special file",
            ));
        }
        Ok(())
    }

    let result = copy_tree(&source, &source, &temporary, &mut BTreeSet::new());
    if let Err(error) = result {
        let _ = fs::remove_dir_all(&temporary);
        return Err(error);
    }
    fs::rename(&temporary, &destination).map_err(|error| {
        let _ = fs::remove_dir_all(&temporary);
        LoomError::new(
            ErrorCode::WorkspaceAccessDenied,
            format!("could not install imported directory: {error}"),
            false,
        )
    })
}

pub(crate) fn copy_filesystem_tree(source: &Path, destination: &Path) -> Result<()> {
    let entries = fs::read_dir(source).map_err(|error| {
        LoomError::new(
            ErrorCode::WorkspaceAccessDenied,
            format!("could not read source session filesystem: {error}"),
            false,
        )
    })?;
    for entry in entries {
        let entry = entry.map_err(|error| {
            LoomError::new(
                ErrorCode::WorkspaceAccessDenied,
                format!("could not inspect source session filesystem: {error}"),
                false,
            )
        })?;
        let source_path = entry.path();
        let destination_path = destination.join(entry.file_name());
        let file_type = entry.file_type().map_err(|error| {
            LoomError::new(
                ErrorCode::WorkspaceAccessDenied,
                format!("could not inspect session filesystem entry: {error}"),
                false,
            )
        })?;
        if file_type.is_dir() {
            fs::create_dir(&destination_path).map_err(|error| {
                LoomError::new(
                    ErrorCode::WorkspaceAccessDenied,
                    format!("could not create copied session directory: {error}"),
                    false,
                )
            })?;
            copy_filesystem_tree(&source_path, &destination_path)?;
        } else if file_type.is_file() {
            fs::copy(&source_path, &destination_path).map_err(|error| {
                LoomError::new(
                    ErrorCode::WorkspaceAccessDenied,
                    format!("could not copy session file: {error}"),
                    false,
                )
            })?;
        } else if file_type.is_symlink() {
            copy_session_symlink(&source_path, &destination_path)?;
        } else {
            return Err(LoomError::new(
                ErrorCode::WorkspaceAccessDenied,
                format!(
                    "cannot copy unsupported filesystem entry '{}'",
                    source_path.display()
                ),
                false,
            ));
        }
    }
    Ok(())
}

#[cfg(unix)]
pub(crate) fn copy_session_symlink(source: &Path, destination: &Path) -> Result<()> {
    let target = fs::read_link(source).map_err(|error| {
        LoomError::new(
            ErrorCode::WorkspaceAccessDenied,
            format!("could not read session symlink: {error}"),
            false,
        )
    })?;
    std::os::unix::fs::symlink(target, destination).map_err(|error| {
        LoomError::new(
            ErrorCode::WorkspaceAccessDenied,
            format!("could not copy session symlink: {error}"),
            false,
        )
    })
}

#[cfg(windows)]
pub(crate) fn copy_session_symlink(source: &Path, destination: &Path) -> Result<()> {
    let target = fs::read_link(source).map_err(|error| {
        LoomError::new(
            ErrorCode::WorkspaceAccessDenied,
            format!("could not read session symlink: {error}"),
            false,
        )
    })?;
    let target_is_dir = fs::metadata(source).is_ok_and(|metadata| metadata.is_dir());
    let result = if target_is_dir {
        std::os::windows::fs::symlink_dir(target, destination)
    } else {
        std::os::windows::fs::symlink_file(target, destination)
    };
    result.map_err(|error| {
        LoomError::new(
            ErrorCode::WorkspaceAccessDenied,
            format!("could not copy session symlink: {error}"),
            false,
        )
    })
}
