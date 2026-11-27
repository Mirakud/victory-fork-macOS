//! A separately attributed, opt-in recovery for one wave32 cascade-shadow program.
//! Only the failed game translation calls this. Positive packs, warm-up and the
//! ordinary converter keep their original contents and build keys.
use super::{Pack, invalid};
use std::fs::File;
use std::io::{self, Read};
use std::path::Path;
use victory_shader::{HwStage, emit::HostShader};

const GUEST: &str = "f133e35efed2a48a319f741044d56733f14f18f1e6145b1a1b9b32976b6326b1";
const REFUSAL: &str = "GFX10 code at 0x1eb4: a lane mask exchanged where not every lane runs is not supported";
// The published translator removes one unused Rust re-export. The D3 and
// public consumers were checked independently on the same D3DMetal device.
const MAC_BUILDS: [&str; 2] = [
    "578b25415d58675d5ba8637836bf4f93baf92f43cb501d6a0365e01c3cec4218",
    "d6b56b368b0f6ce27b5b819dc4b894255b9ef61d1933c67010652f6a2cb6bb62",
];
// The native Belobug 474ff19 preparation, with wave 32 and null view 999999.
// The original VSP checksum covers its build identity, the full guest hash,
// every binding/sampler and every DXIL byte; an altered pack is ineligible.
const BELOBUG_BUILD: &str = "fbdf3914296075dfdd0503c9c351d6cbc23f80f4f9118bd1c2a76b30f0b09c3f";
const PACK_CHECKSUM: &str = "2535a3668b70f7fb0d316174449c5dcc8ed31eb2828e0a679ce17a37e1e46d65";
const PACK_SIZE: usize = 666380;

fn load(path: &Path, guest: &blake3::Hash) -> io::Result<HostShader> {
    let file = File::open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() != PACK_SIZE as u64 {
        return Err(invalid("wrong cascade pack size"));
    }
    let mut data = Vec::with_capacity(PACK_SIZE);
    file.take((PACK_SIZE + 1) as u64).read_to_end(&mut data)?;
    if data.len() != PACK_SIZE || !data.ends_with(blake3::Hash::from_hex(PACK_CHECKSUM).unwrap().as_bytes()) {
        return Err(invalid("wrong Belobug cascade pack checksum"));
    }
    let pack = Pack::parse(data)?; // Also checks the checksum against the actual contents.
    if blake3::Hash::from(pack.build).to_hex().as_str() != BELOBUG_BUILD || pack.index.len() != 1 {
        return Err(invalid("wrong Belobug cascade producer"));
    }
    pack.shader(guest.as_bytes()).filter(|s| s.stage == HwStage::Cs)
        .ok_or_else(|| invalid("missing Belobug cascade compute record"))
}

pub(crate) fn recover(container: &[u8], error: String) -> Result<HostShader, String> {
    if error.strip_prefix("converter shader.dxil v1: ").unwrap_or(&error) != REFUSAL {
        return Err(error);
    }
    let Some(path) = std::env::var_os("VICTORY_BELOBUG_CASCADE_PACK") else { return Err(error) };
    let guest = blake3::hash(container);
    if guest.to_hex().as_str() != GUEST {
        return Err(error);
    }
    let build = blake3::Hash::from(super::super::build());
    if !MAC_BUILDS.contains(&build.to_hex().as_str()) {
        tracing::warn!(%build, "cascade recovery unavailable for these host or translator settings");
        return Err(error);
    }
    match load(Path::new(&path), &guest) {
        Ok(shader) => {
            tracing::info!(%guest, producer = BELOBUG_BUILD, %error, "using Belobug cascade compute after standard refusal");
            Ok(shader)
        }
        Err(reason) => {
            tracing::warn!(%reason, "Belobug cascade compute unavailable");
            Err(error)
        }
    }
}

#[cfg(test)]
pub(crate) fn consumer_build() -> blake3::Hash {
    super::super::build().into()
}
