//! Validated wave32 prefix scans for XboxBvhBuilder, keyed by the entire guest
//! container, host translation settings and complete VSH1 record.
use std::path::PathBuf;
use victory_shader::{HwStage, emit::HostShader};

const BUILD: &str = "578b25415d58675d5ba8637836bf4f93baf92f43cb501d6a0365e01c3cec4218";
const SCANS: [(&str, &str); 3] = [
    (
        "39321a95223001ed83aac979ffb2b7beb12e3e46e3e30d3fe88e30ecb51f1799",
        "fcab64a24865f587125dc24e06e774d85f3ee5307e0dbcd4431c599340d94b28",
    ),
    (
        "77360df21e12ea433054eadc1ed1eeda74cf05f670f6645d1694847062119c44",
        "3e5d4c8b05b6a7bd871572221d9712f8fb25ecbe19b1445ceff3e93a092f228f",
    ),
    (
        "9fc111d3be79364fe79f038ab5201d2cdf4546eb9c4a7a81469e79de4980fff5",
        "f37a97fb335674b725479ee221d66da413f5cf3dba6c12a61d3275b16f9b7a26",
    ),
];

pub(crate) fn recover(container: &[u8], error: String) -> Result<HostShader, String> {
    let Some(directory) = std::env::var_os("VICTORY_BVH_SCAN_RECORDS") else {
        return Err(error);
    };
    let guest = blake3::hash(container).to_hex();
    let Some((_, expected)) = SCANS.iter().find(|(id, _)| *id == guest.as_str()) else {
        return Err(error);
    };
    let build = blake3::Hash::from(super::super::build());
    if build.to_hex().as_str() != BUILD {
        return Err(format!("{error}; BVH scan unavailable for translator {build}"));
    }
    let path = PathBuf::from(directory).join(format!("{guest}.vsh"));
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(reason) => {
            return Err(format!("{error}; BVH scan {guest} unavailable: {reason}"));
        }
    };
    if blake3::hash(&bytes).to_hex().as_str() != *expected {
        return Err(error);
    }
    let host = super::super::decode(&bytes)
        .filter(|h| h.stage == HwStage::Cs)
        .ok_or(error)?;
    tracing::info!(target: "victory_gpu::memory_watch", %guest, "using verified BVH prefix scan");
    Ok(host)
}
