const WINDOW: u64 = 64;

/// Schiebefenster gegen Wiederholungen; verlorene und vertauschte Pakete innerhalb des Fensters sind erlaubt.
#[derive(Debug, Default)]
pub struct ReplayWindow {
    highest: Option<u64>,
    seen: u64,
}

impl ReplayWindow {
    pub fn new() -> Self {
        Self::default()
    }

    /// Prüft, ob die Nummer akzeptabel wäre. Ändert den Zustand nicht.
    pub fn check(&self, seq: u64) -> bool {
        // u64::MAX ist in Noise als Nonce reserviert.
        if seq == u64::MAX {
            return false;
        }
        match self.highest {
            None => true,
            Some(h) if seq > h => true,
            Some(h) => {
                let diff = h - seq;
                diff < WINDOW && self.seen & (1 << diff) == 0
            }
        }
    }

    /// Markiert die Nummer als gesehen. Erst nach erfolgreicher Authentifizierung aufrufen.
    pub fn update(&mut self, seq: u64) {
        match self.highest {
            Some(h) if seq <= h => {
                let diff = h - seq;
                if diff < WINDOW {
                    self.seen |= 1 << diff;
                }
            }
            Some(h) => {
                let shift = seq - h;
                self.seen = if shift >= WINDOW { 0 } else { self.seen << shift };
                self.seen |= 1;
                self.highest = Some(seq);
            }
            None => {
                self.highest = Some(seq);
                self.seen = 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn accept(w: &mut ReplayWindow, seq: u64) -> bool {
        let ok = w.check(seq);
        if ok {
            w.update(seq);
        }
        ok
    }

    #[test]
    fn rejects_duplicates() {
        let mut w = ReplayWindow::new();
        assert!(accept(&mut w, 0));
        assert!(!accept(&mut w, 0));
        assert!(accept(&mut w, 1));
        assert!(!accept(&mut w, 1));
    }

    #[test]
    fn accepts_gaps_and_reordering_inside_window() {
        let mut w = ReplayWindow::new();
        assert!(accept(&mut w, 10));
        assert!(accept(&mut w, 50));
        assert!(accept(&mut w, 30));
        assert!(!accept(&mut w, 30));
        assert!(accept(&mut w, 11));
    }

    #[test]
    fn rejects_too_old() {
        let mut w = ReplayWindow::new();
        assert!(accept(&mut w, 100));
        assert!(!accept(&mut w, 36));
        assert!(accept(&mut w, 37));
    }

    #[test]
    fn large_jump_clears_window() {
        let mut w = ReplayWindow::new();
        assert!(accept(&mut w, 5));
        assert!(accept(&mut w, 5 + 1000));
        assert!(!accept(&mut w, 5));
        assert!(!accept(&mut w, 1005));
    }

    #[test]
    fn check_does_not_modify_state() {
        let mut w = ReplayWindow::new();
        assert!(w.check(3));
        assert!(w.check(3));
        w.update(3);
        assert!(!w.check(3));
    }

    #[test]
    fn rejects_reserved_max_nonce() {
        assert!(!ReplayWindow::new().check(u64::MAX));
    }
}
