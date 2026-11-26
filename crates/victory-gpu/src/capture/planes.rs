//! Read the original resources on the presentation list, before the overlay.
use super::{folder, read_back};
use crate::host::transition;
use std::io::{self, Write};
use std::time::{SystemTime, UNIX_EPOCH};
use windows::Win32::Graphics::Direct3D12::*;
use windows::core::Interface;
#[path = "plane_image.rs"]
mod image;

pub(crate) struct Plane {
    guest: usize,
    host: usize,
    state: i32,
    desc: D3D12_RESOURCE_DESC,
    format: i32,
    bytes: Option<(ID3D12Resource, D3D12_PLACED_SUBRESOURCE_FOOTPRINT)>,
}

impl Plane {
    pub(crate) fn record(
        device: &ID3D12Device,
        list: &ID3D12GraphicsCommandList,
        texture: &ID3D12Resource,
        guest: usize,
        state: D3D12_RESOURCE_STATES,
    ) -> Self {
        // SAFETY: a live, borrowed input plane on the presentation list.
        let desc = unsafe { texture.GetDesc() };
        let format = crate::host::swap_format(desc.Format).map_or(0, |f| f.0);
        let bytes = if image::extension(format).is_some() && desc.SampleDesc.Count == 1 {
            let src = D3D12_RESOURCE_STATE_COPY_SOURCE;
            let move_to_copy = (state.0 & src.0) == 0;
            // SAFETY: barriers are on the same list as the copy and restore the initial state.
            unsafe {
                if move_to_copy {
                    list.ResourceBarrier(&[transition(texture, state, src)]);
                }
                let read = read_back(device, list, texture);
                if move_to_copy {
                    list.ResourceBarrier(&[transition(texture, src, state)]);
                }
                read
            }
        } else {
            None
        };
        Self {
            guest,
            host: texture.as_raw() as usize,
            state: state.0,
            desc,
            format,
            bytes,
        }
    }

    fn save(&self, base: &std::path::Path) -> io::Result<()> {
        let (buffer, fp) = self
            .bytes
            .as_ref()
            .ok_or_else(|| io::Error::other("plane readback unavailable"))?;
        let mut data = std::ptr::null_mut();
        // SAFETY: the synchronous present list has completed. Map only the size of the
        // actual resource (the last row need not contain the trailing pitch padding).
        unsafe {
            buffer
                .Map(0, None, Some(&mut data))
                .map_err(io::Error::other)?;
            let rows =
                std::slice::from_raw_parts(data.cast::<u8>(), buffer.GetDesc().Width as usize);
            let result = (|| {
                let mut file = io::BufWriter::new(std::fs::File::create(base)?);
                let f = fp.Footprint;
                image::write_image(
                    &mut file,
                    self.format,
                    f.Width,
                    f.Height,
                    f.RowPitch as usize,
                    &rows[fp.Offset as usize..],
                )?;
                file.flush()
            })();
            buffer.Unmap(0, None);
            result
        }
    }
}

pub(crate) fn timestamp_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

pub(crate) fn save(
    frame: u64,
    timestamp: u128,
    primary: Plane,
    overlay: Option<Plane>,
    overlay_guest: usize,
    overlay_drawn: bool,
) {
    let Some(dir) = folder() else { return };
    let mut meta = String::from(
        "frame\tpresent_unix_ms\tplane\tguest\thost\twidth\theight\tresource_format\tview_format\tcopy_state\tapplied\tfile\tstatus\n",
    );
    for (role, plane) in [("primary", Some(primary)), ("overlay", overlay)] {
        let Some(p) = plane else {
            let reason = if overlay_guest == 0 {
                "absent or same as primary"
            } else {
                "no host texture"
            };
            meta.push_str(&format!("{frame}\t{timestamp}\t{role}\t{overlay_guest:#x}\t-\t-\t-\t-\t-\t-\tfalse\t-\t{reason}\n"));
            continue;
        };
        let name = format!(
            "plane-{frame}-{role}.{}",
            image::extension(p.format).unwrap_or("unsupported")
        );
        let path = dir.join(&name);
        let status = match p.save(&path) {
            Ok(()) => "ok".to_owned(),
            Err(e) => {
                let _ = std::fs::remove_file(&path);
                tracing::warn!(frame, role, error = %e, "present plane not saved");
                e.to_string()
            }
        };
        let applied = role == "primary" || overlay_drawn;
        meta.push_str(&format!("{frame}\t{timestamp}\t{role}\t{:#x}\t{:#x}\t{}\t{}\t{}\t{}\t{:#x}\t{applied}\t{name}\t{status}\n", p.guest,p.host,p.desc.Width,p.desc.Height,p.desc.Format.0,p.format,p.state));
    }
    if let Err(e) = std::fs::write(dir.join(format!("planes-{frame}.tsv")), meta) {
        tracing::warn!(frame,error=%e,"present-plane metadata not saved");
    }
}
