use std::path::PathBuf;

use prefix::Prefix;
use prefix::format::manifest::{Entry, Kind, Manifest};
use prefix::format::sha256_hex;
use prefix::state::{Base, State, Trust};
use prefix::system::{self, SystemFiles};

/// A prefix whose base is `/usr/lib/libSystem.B.dylib`, under `policy`, set as
/// this process's prefix.
pub fn provisioned(name: &str, policy: SystemFiles) -> Prefix {
    let at: PathBuf =
        std::env::temp_dir().join(format!("rosemary-guard-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&at);
    prefix::layout::create(&at).unwrap();
    let prefix = Prefix::at(&at);
    let content = b"\xcf\xfa\xed\xfe";
    let manifest = Manifest::new(
        [
            ("arch".to_string(), "x86_64".to_string()),
            ("project".to_string(), "minidarwin".to_string()),
        ]
        .into(),
        None,
        vec![
            Entry {
                path: "/usr".into(),
                kind: Kind::Directory {
                    mode: 0o755,
                    digest: String::new(),
                },
            },
            Entry {
                path: "/usr/lib".into(),
                kind: Kind::Directory {
                    mode: 0o755,
                    digest: String::new(),
                },
            },
            Entry {
                path: "/usr/lib/libSystem.B.dylib".into(),
                kind: Kind::File {
                    mode: 0o755,
                    size: 4,
                    sha256: sha256_hex(content),
                },
            },
        ],
    );
    std::fs::write(prefix.drive().join("usr/lib/libSystem.B.dylib"), content).unwrap();
    let text = manifest.render();
    std::fs::write(prefix.control().join("manifest.yaml"), &text).unwrap();
    let spec = system::with_policy(prefix::pinned::text("x86_64").unwrap(), policy);
    std::fs::write(prefix.control().join("spec.yaml"), spec).unwrap();
    let state = State {
        runtime_version: None,
        base: Some(Base {
            project: "minidarwin".into(),
            arch: "x86_64".into(),
            sequence: None,
            tag: None,
            tree: manifest.tree.clone(),
            manifest_sha256: sha256_hex(text.as_bytes()),
            artifact_sha256: None,
            trust: Trust::User,
            policy: prefix::format::spec::Policy::Strict,
            materialised: vec!["copy".into()],
        }),
        accepted_sequence: None,
        skip_update_prompts: false,
    };
    state.write(&prefix::layout::state_path(&at)).unwrap();
    // SAFETY: before the namespace or any other thread reads the environment.
    unsafe { std::env::set_var("ROSEMARY_PREFIX", &at) };
    prefix
}
