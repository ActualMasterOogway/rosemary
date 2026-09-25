use std::collections::BTreeMap;
use std::path::Path;

fn main() {
    let directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/specs");
    println!("cargo::rerun-if-changed={}", directory.display());
    let mut found: BTreeMap<String, String> = BTreeMap::new();
    for entry in std::fs::read_dir(&directory).expect("src/specs exists") {
        let entry = entry.expect("src/specs is readable");
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(stem) = name.strip_suffix(".spec.yaml") else {
            continue;
        };
        // minidarwin-rootfs-<arch>-r<N>
        let arch = stem
            .strip_prefix("minidarwin-rootfs-")
            .and_then(|rest| rest.rsplit_once("-r"))
            .map(|(arch, _)| arch.to_string())
            .unwrap_or_else(|| {
                panic!("src/specs/{name}: not a minidarwin-rootfs-<arch>-r<N> spec")
            });
        if let Some(other) = found.insert(arch.clone(), name.clone()) {
            panic!("src/specs: both {other} and {name} are for {arch}; delete the older one");
        }
    }
    let mut out = String::from("pub static ALL: &[(&str, &str)] = &[\n");
    for (arch, name) in &found {
        let path = directory.join(name);
        out.push_str(&format!(
            "    ({arch:?}, include_str!({:?})),\n",
            path.display().to_string()
        ));
    }
    out.push_str("];\n");
    let generated =
        Path::new(&std::env::var("OUT_DIR").expect("cargo sets OUT_DIR")).join("pinned.rs");
    std::fs::write(generated, out).expect("OUT_DIR is writable");
}
