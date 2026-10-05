//! QR image for a portable invite code (freenet/river#741).
//!
//! The matrix is the code itself, not the host-baked invite link. Two phones
//! in the same room are often on different hosts (a local node and another
//! peer); the link's host would be wrong for the person scanning it. The code
//! is what "Enter Invite Code" already accepts.

use qrcode::types::Color;
use qrcode::{EcLevel, QrCode};

const QUIET_ZONE: usize = 4;

/// `true` is a dark module. The quiet zone is included, so the scanner sees a
/// white margin even when the surrounding panel is dark.
pub(crate) fn invitation_qr_matrix(code: &str) -> Result<Vec<Vec<bool>>, String> {
    if code.is_empty() {
        return Err("There is no invite code to show.".to_string());
    }
    let qr = QrCode::with_error_correction_level(code.as_bytes(), EcLevel::M)
        .map_err(|_| "This invitation is too long to show as a QR code.".to_string())?;
    let modules = qr.width();
    let side = modules + QUIET_ZONE * 2;
    let mut rows = vec![vec![false; side]; side];
    for y in 0..modules {
        for x in 0..modules {
            if qr[(x, y)] == Color::Dark {
                rows[y + QUIET_ZONE][x + QUIET_ZONE] = true;
            }
        }
    }
    Ok(rows)
}

/// SVG markup for [`invitation_qr_matrix`]. One path, so the modal does not
/// mount a DOM node per module. The markup contains only coordinates we
/// generated; the invite code itself is not interpolated into it.
pub(crate) fn invitation_qr_svg(code: &str) -> Result<String, String> {
    let modules = invitation_qr_matrix(code)?;
    Ok(matrix_to_svg(&modules))
}

fn matrix_to_svg(modules: &[Vec<bool>]) -> String {
    let side = modules.len();
    let mut path = String::new();
    for (y, row) in modules.iter().enumerate() {
        for (x, dark) in row.iter().enumerate() {
            if *dark {
                path.push_str(&format!("M{x} {y}h1v1H{x}z"));
            }
        }
    }
    format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 {side} {side}\" \
         shape-rendering=\"crispEdges\" role=\"img\" aria-label=\"Invitation QR code\" \
         style=\"width:100%;height:auto;display:block\"><rect width=\"{side}\" height=\"{side}\" \
         fill=\"#ffffff\"/><path fill=\"#000000\" d=\"{path}\"/></svg>"
    )
}

/// Text a scan or a paste should be decoded as.
///
/// The QR we draw is the bare code. A full invite link is accepted too, so a
/// scan of the link (or a paste of it) still reaches the same code the
/// `?invitation=` parameter carries.
pub(crate) fn invitation_text_from_scan(raw: &str) -> String {
    let trimmed = raw.trim();
    invitation_query_param(trimmed).unwrap_or_else(|| trimmed.to_string())
}

fn invitation_query_param(text: &str) -> Option<String> {
    let query = text.split_once('?')?.1.split('#').next().unwrap_or("");
    for pair in query.split('&') {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        if key == "invitation" && !value.is_empty() {
            return Some(percent_decode(value));
        }
    }
    None
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            if let Ok(hex) = std::str::from_utf8(&bytes[index + 1..index + 3]) {
                if let Ok(value) = u8::from_str_radix(hex, 16) {
                    out.push(value);
                    index += 3;
                    continue;
                }
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::{invitation_qr_matrix, invitation_qr_svg, invitation_text_from_scan};
    use crate::components::members::Invitation;
    use ed25519_dalek::SigningKey;
    use image::{GrayImage, Luma};
    use river_core::room_state::member::{AuthorizedMember, Member, MemberId};

    fn sample_invitation_code() -> String {
        let inviter = SigningKey::from_bytes(&[7u8; 32]);
        let invitee = SigningKey::from_bytes(&[8u8; 32]);
        let owner = SigningKey::from_bytes(&[9u8; 32]);
        let member = Member {
            owner_member_id: MemberId::from(&owner.verifying_key()),
            invited_by: MemberId::from(&inviter.verifying_key()),
            member_vk: invitee.verifying_key(),
        };
        Invitation {
            room: owner.verifying_key(),
            invitee_signing_key: invitee,
            invitee: AuthorizedMember::new(member, &inviter),
            room_secrets: vec![(0, [0x11; 32]), (1, [0x22; 32])],
        }
        .to_encoded_string()
    }

    fn decode_modules(modules: &[Vec<bool>]) -> Result<String, String> {
        // Eight pixels per module. The matrix already has the QR quiet zone;
        // rqrr still misses a code that starts on the first pixel, so the
        // raster adds a further white margin the on-screen SVG does not need
        // (the modal paints that margin as padding).
        const SCALE: u32 = 8;
        const MARGIN: u32 = 32;
        let modules_side = modules.len() as u32;
        let side = modules_side * SCALE + MARGIN * 2;
        let mut image = GrayImage::new(side, side);
        for pixel in image.pixels_mut() {
            *pixel = Luma([255]);
        }
        for (y, row) in modules.iter().enumerate() {
            for (x, dark) in row.iter().enumerate() {
                if !dark {
                    continue;
                }
                let origin_x = MARGIN + x as u32 * SCALE;
                let origin_y = MARGIN + y as u32 * SCALE;
                for dy in 0..SCALE {
                    for dx in 0..SCALE {
                        image.put_pixel(origin_x + dx, origin_y + dy, Luma([0]));
                    }
                }
            }
        }
        let mut prepared = rqrr::PreparedImage::prepare(image);
        let grids = prepared.detect_grids();
        let grid = grids
            .into_iter()
            .next()
            .ok_or_else(|| format!("no QR grid in a {modules_side}-module code"))?;
        let (_meta, content) = grid
            .decode()
            .map_err(|err| format!("QR decode failed: {err}"))?;
        Ok(content)
    }

    #[test]
    fn a_real_invitation_qr_decodes_to_that_invite_code() {
        let code = sample_invitation_code();
        let modules = invitation_qr_matrix(&code).expect("invitation fits in a QR code");
        // Version 30 is 137 modules. Past that, a phone held at the other
        // phone's screen stops resolving the modules.
        assert!(
            modules.len() <= 137 + 8,
            "invite QR is {} modules across",
            modules.len()
        );
        let decoded = decode_modules(&modules).expect("raster of the matrix decodes");
        assert_eq!(decoded, code);
    }

    #[test]
    fn the_svg_uses_the_same_matrix_the_decoder_reads() {
        let code = sample_invitation_code();
        let modules = invitation_qr_matrix(&code).unwrap();
        let svg = invitation_qr_svg(&code).unwrap();
        let side = modules.len();
        assert!(svg.contains(&format!("viewBox=\"0 0 {side} {side}\"")));
        assert!(svg.contains("Invitation QR code"));
        // A dark module's path command is present, so the SVG is that matrix
        // and not an empty white square of the same size.
        let (x, y) = modules
            .iter()
            .enumerate()
            .find_map(|(y, row)| row.iter().position(|dark| *dark).map(|x| (x, y)))
            .expect("a QR code has dark modules");
        assert!(svg.contains(&format!("M{x} {y}h1v1H{x}z")));
    }

    #[test]
    fn a_scan_of_the_bare_code_and_of_an_invite_link_both_yield_the_code() {
        let code = sample_invitation_code();
        assert_eq!(invitation_text_from_scan(&code), code);
        assert_eq!(invitation_text_from_scan(&format!("\n{code} \n")), code);
        let link = format!(
            "http://127.0.0.1:7509/v1/contract/web/CONTRACT_ID/?invitation={code}&from=qr#ignored"
        );
        assert_eq!(invitation_text_from_scan(&link), code);
        assert_eq!(
            invitation_text_from_scan("https://peer.example/?invitation=Ab%43"),
            "AbC"
        );
        assert_eq!(invitation_text_from_scan("   "), "");
        assert_eq!(
            invitation_text_from_scan("https://peer.example/?foo=1"),
            "https://peer.example/?foo=1"
        );
    }
}
