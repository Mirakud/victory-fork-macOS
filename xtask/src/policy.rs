//! Rule #1, statically: the repository only ever contains small text files
//! that we wrote. See docs/asset-policy.md.
//!
//! This is an allowlist, not a blocklist: a file passes only if its name is
//! on the list, it is not too big, it is text, and it does not smuggle data
//! inside text (long hex/base64 runs).

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};

const MAX_FILE_SIZE: usize = 512 * 1024;
const MAX_ENCODED_RUN: usize = 512;

const ALLOWED_EXTENSIONS: &[&str] = &[
    "rs", "toml", "md", "txt", "lock", "yml", "yaml", "json", "hlsl", "hlsli", "ps1", "cmd", "sh",
];
const ALLOWED_NAMES: &[&str] = &[
    ".gitignore",
    ".gitattributes",
    ".editorconfig",
    ".victory-root",
    "pre-commit",
    "LICENSE",
    "LICENSE-MIT",
    "LICENSE-APACHE",
];

/// Paths explicitly exempted, with a reason. Keep this empty if at all possible.
const EXCEPTIONS: &[(&str, &str)] = &[(
    "crates/victory-formats/src/bcpack-tables.bin",
    "the BCPack format's constant tables: nothing decodes without them and they cannot be derived (owner's decision, 2026-11-23; docs/asset-policy.md)",
)];

/// Signatures of formats that must never be committed.
const FORBIDDEN_MAGIC: &[(&[u8], &str)] = &[
    (b"MZ", "PE executable"),
    (b"DXBC", "shader container"),
    (b"DDS ", "DDS texture"),
    (b"RIFF", "RIFF (audio/video)"),
    (b"BC\xC0\xDE", "LLVM bitcode"),
    (b"Microsoft C/C++ MSF", "PDB"),
    (b"!<arch>", "library archive"),
    (b"PK\x03\x04", "zip"),
    (b"7z\xBC\xAF", "7-zip"),
    (b"MSCF", "cabinet"),
    (b"\xD0\xCF\x11\xE0", "MSI/OLE"),
];

pub fn run(root: &Path, staged: bool) -> Result<()> {
    let files = list_files(root, staged)?;
    let mut violations = Vec::new();
    for path in &files {
        if EXCEPTIONS.iter().any(|(p, _)| p == path) {
            continue;
        }
        let content = if staged {
            let out = Command::new("git")
                .args(["show", &format!(":{path}")])
                .current_dir(root)
                .output()?;
            if !out.status.success() {
                continue; // deleted in the index
            }
            out.stdout
        } else {
            match std::fs::read(root.join(path)) {
                Ok(c) => c,
                Err(_) => continue,
            }
        };
        for problem in check(path, &content) {
            violations.push(format!("{path}: {problem}"));
        }
    }
    if violations.is_empty() {
        println!("asset policy: {} files ok", files.len());
        return Ok(());
    }
    for v in &violations {
        eprintln!("POLICY: {v}");
    }
    bail!(
        "{} asset-policy violation(s). Nothing from the game may be committed; \
         see docs/asset-policy.md",
        violations.len()
    )
}

fn list_files(root: &Path, staged: bool) -> Result<Vec<String>> {
    let git = |args: &[&str]| -> Result<Option<Vec<String>>> {
        let out = Command::new("git")
            .args(args)
            .current_dir(root)
            .output()
            .context("running git")?;
        if !out.status.success() {
            return Ok(None);
        }
        Ok(Some(
            out.stdout
                .split(|&b| b == 0)
                .filter(|s| !s.is_empty())
                .map(|s| String::from_utf8_lossy(s).replace('\\', "/"))
                .collect(),
        ))
    };
    if staged {
        return git(&[
            "diff",
            "--cached",
            "--name-only",
            "--diff-filter=ACMR",
            "-z",
        ])?
        .context("--staged needs a git repository");
    }
    if let Some(mut tracked) = git(&[
        "ls-files",
        "-z",
        "--cached",
        "--others",
        "--exclude-standard",
    ])? {
        tracked.sort();
        tracked.dedup();
        return Ok(tracked);
    }
    // Not a git repository: walk everything except build output.
    Ok(walkdir::WalkDir::new(root)
        .into_iter()
        .filter_entry(|e| !matches!(e.file_name().to_str(), Some("target" | ".git")))
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file())
        .filter_map(|e| {
            e.path()
                .strip_prefix(root)
                .ok()
                .map(|p| p.to_string_lossy().replace('\\', "/"))
        })
        .collect())
}

/// All problems with one file. Pure, for testing.
pub fn check(path: &str, content: &[u8]) -> Vec<String> {
    let mut problems = Vec::new();
    let name = path.rsplit('/').next().unwrap_or(path);
    let ext = name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase());
    // Only the reviewed PNGs shown by the Mac fork's README.
    // Original Retina captures supplied by the player on macOS.
    let retina_capture = matches!(
        path,
        "docs/macos/030001-prison-cour.png"
            | "docs/macos/030001-prison-profil.png"
            | "docs/macos/menu-game-macos.png"
    );
    let reviewed_capture = matches!(
        path,
        "docs/macos/rue-1.png"
            | "docs/macos/rue-2.png"
            | "docs/macos/rue-3.png"
            | "docs/macos/rue-profondeur.png"
            | "docs/macos/voiture.png"
            | "docs/macos/frame-1808.png"
            | "docs/macos/frame-1816.png"
            | "docs/macos/frame-1824.png"
            | "docs/macos/frame-1832.png"
            | "docs/macos/frame-1840.png"
            | "docs/macos/frame-1848.png"
            | "docs/macos/frame-1856.png"
            | "docs/macos/frame-1864.png"
            | "docs/macos/frame-1872.png"
            | "docs/macos/encart-monde.png"
            | "docs/macos/202903-frame-1662.png"
            | "docs/macos/202903-frame-1670.png"
            | "docs/macos/202903-frame-1712.png"
            | "docs/macos/011924-frame-2410.png"
            | "docs/macos/011924-frame-2518.png"
            | "docs/macos/011924-frame-2542.png"
            | "docs/macos/visage-et-cou-macos.png"
    );
    if (reviewed_capture && content.len() <= MAX_FILE_SIZE || retina_capture && content.len() <= 5 * 1024 * 1024)
        && content.starts_with(b"\x89PNG\r\n\x1a\n")
        && content.ends_with(b"IEND\xae\x42\x60\x82")
    {
        return problems;
    }
    if path == "docs/macos/cou-en-jeu-avant-apres.jpg" && content.len() <= MAX_FILE_SIZE
        && content.starts_with(&[0xff, 0xd8, 0xff]) && content.ends_with(&[0xff, 0xd9])
    {
        return problems;
    }
    let mac_tool = (path.starts_with("tools/macos/") || path.starts_with("tools/shaders/"))
        && ext.as_deref() == Some("py");
    let allowed = matches!(path, "tools/run_victory_d3dmetal4_mac.command" | "tools/run_victory_prison_avec_hud_mac.command")
        || path == "tools/macos/victory_perf_hud.swift"
        || mac_tool
        || ALLOWED_NAMES.contains(&name)
        || ext
            .as_deref()
            .is_some_and(|e| ALLOWED_EXTENSIONS.contains(&e));
    if !allowed {
        problems.push("file type not on the allowlist".to_owned());
    }
    if content.len() > MAX_FILE_SIZE {
        problems.push(format!(
            "{} bytes exceeds the {MAX_FILE_SIZE}-byte limit",
            content.len()
        ));
    }
    for (magic, what) in FORBIDDEN_MAGIC {
        if content.starts_with(magic) {
            problems.push(format!("looks like a {what}"));
        }
    }
    if content.contains(&0) {
        problems.push("binary content (NUL bytes)".to_owned());
    } else if let Some(run) = longest_encoded_run(content)
        && run > MAX_ENCODED_RUN
    {
        problems.push(format!("{run}-character hex/base64 run (embedded data?)"));
    }
    problems
}

/// Longest run of characters that could be hex or base64 payload.
fn longest_encoded_run(content: &[u8]) -> Option<usize> {
    let is_payload = |b: &u8| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'=');
    content.split(|b| !is_payload(b)).map(<[u8]>::len).max()
}

#[cfg(test)]
mod tests {
    use super::check;

    /// Enforces the policy wherever tests run, even in clones without the hook.
    #[test]
    fn repository_passes_policy() {
        super::run(&crate::workspace_root(), false).unwrap();
    }

    #[test]
    fn accepts_source() {
        assert!(check("crates/x/src/lib.rs", b"fn main() {}\n").is_empty());
        assert!(check(".gitignore", b"/target/\n").is_empty());
        assert!(check(".githooks/pre-commit", b"#!/bin/sh\n").is_empty());
    }

    #[test]
    fn rejects_game_like_files() {
        assert!(!check("assets/tex.dds", b"DDS \x7c\0\0\0").is_empty());
        assert!(
            !check("notes.txt", b"MZ\x90\0").is_empty(),
            "magic wins over extension"
        );
        assert!(
            !check("data.bin", b"hello").is_empty(),
            "extension not allowed"
        );
        assert!(
            !check("a.rs", &vec![b'a'; 600 * 1024]).is_empty(),
            "too big"
        );
    }

    #[test]
    fn rejects_data_smuggled_into_text() {
        let blob = format!("const DATA: &str = \"{}\";", "A1".repeat(400));
        assert!(!check("src/blob.rs", blob.as_bytes()).is_empty());
    }
}
