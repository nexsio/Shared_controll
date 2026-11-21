//! Zerlegung von Frames in 64-Byte-HID-Berichte und Wiederzusammensetzung.

use transport::TransportError;

pub const REPORT_LEN: usize = 64;
pub const TYPE_DATA: u8 = 1;
pub const TYPE_STATUS: u8 = 2;
pub const TYPE_LINK: u8 = 3;

// Kopf eines Datenberichts nach dem Typ-Byte: Frame-Id, Index, Anzahl, Länge des Stücks.
const HEADER_LEN: usize = 4;
const CHUNK_LEN: usize = REPORT_LEN - 1 - HEADER_LEN;
const MAX_CHUNKS: usize = 18;
pub const MAX_FRAME_LEN: usize = MAX_CHUNKS * CHUNK_LEN;

pub fn fragment(frame_id: u8, frame: &[u8]) -> Result<Vec<[u8; REPORT_LEN]>, TransportError> {
    if frame.len() > MAX_FRAME_LEN {
        return Err(TransportError::FrameTooLarge { max: MAX_FRAME_LEN });
    }
    let total = frame.len().div_ceil(CHUNK_LEN).max(1);
    let mut reports = Vec::with_capacity(total);
    for index in 0..total {
        let start = index * CHUNK_LEN;
        let chunk = &frame[start..(start + CHUNK_LEN).min(frame.len())];
        let mut report = [0u8; REPORT_LEN];
        report[0] = TYPE_DATA;
        report[1] = frame_id;
        report[2] = index as u8;
        report[3] = total as u8;
        report[4] = chunk.len() as u8;
        report[5..5 + chunk.len()].copy_from_slice(chunk);
        reports.push(report);
    }
    Ok(reports)
}

#[derive(Default)]
pub struct Reassembler {
    current: Option<Partial>,
}

struct Partial {
    id: u8,
    total: u8,
    next: u8,
    data: Vec<u8>,
}

impl Reassembler {
    /// Liefert den Frame, sobald sein letztes Stück eintrifft. Fehlt ein Stück, wird der Frame verworfen.
    pub fn push(&mut self, report: &[u8; REPORT_LEN]) -> Option<Vec<u8>> {
        if report[0] != TYPE_DATA {
            return None;
        }
        let (id, index, total, len) = (report[1], report[2], report[3], report[4] as usize);
        if total == 0 || index >= total || len > CHUNK_LEN || (index + 1 != total && len != CHUNK_LEN) {
            self.current = None;
            return None;
        }
        if index == 0 {
            self.current = Some(Partial { id, total, next: 0, data: Vec::new() });
        }
        let partial = self.current.as_mut()?;
        if partial.id != id || partial.total != total || partial.next != index {
            self.current = None;
            return None;
        }
        partial.data.extend_from_slice(&report[5..5 + len]);
        partial.next += 1;
        if index + 1 == total {
            return self.current.take().map(|p| p.data);
        }
        None
    }

    pub fn reset(&mut self) {
        self.current = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reassemble(reports: &[[u8; REPORT_LEN]]) -> Vec<Vec<u8>> {
        let mut reassembler = Reassembler::default();
        reports.iter().filter_map(|r| reassembler.push(r)).collect()
    }

    #[test]
    fn frames_survive_roundtrip_at_chunk_boundaries() {
        for len in [0, 1, CHUNK_LEN - 1, CHUNK_LEN, CHUNK_LEN + 1, 2 * CHUNK_LEN, MAX_FRAME_LEN] {
            let frame: Vec<u8> = (0..len).map(|i| i as u8).collect();
            let reports = fragment(7, &frame).unwrap();
            assert_eq!(reassemble(&reports), vec![frame]);
        }
    }

    #[test]
    fn oversized_frame_is_rejected() {
        let frame = vec![0u8; MAX_FRAME_LEN + 1];
        assert!(matches!(fragment(0, &frame), Err(TransportError::FrameTooLarge { max: MAX_FRAME_LEN })));
    }

    #[test]
    fn missing_chunk_drops_frame_but_next_frame_arrives() {
        let lost = fragment(1, &vec![9u8; 3 * CHUNK_LEN]).unwrap();
        let next = fragment(2, b"ok").unwrap();
        let mut reports = vec![lost[0], lost[2]];
        reports.extend(next);
        assert_eq!(reassemble(&reports), vec![b"ok".to_vec()]);
    }

    #[test]
    fn non_data_reports_are_ignored() {
        let mut link = [0u8; REPORT_LEN];
        link[0] = TYPE_LINK;
        link[1] = 1;
        assert!(reassemble(&[link]).is_empty());
    }
}
