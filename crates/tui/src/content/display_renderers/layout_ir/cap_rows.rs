#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum CapRow {
    Child(usize),
    Marker {
        skipped: usize,
        kept: usize,
        total: Option<u64>,
        direction: &'static str,
    },
}

#[derive(Clone, Copy)]
enum CapSegment {
    Children {
        start: usize,
        count: usize,
    },
    Marker {
        skipped: usize,
        kept: usize,
        total: Option<u64>,
        direction: &'static str,
        omitted: Option<(usize, usize)>,
    },
}

#[derive(Clone, Copy, Default)]
pub(super) struct CapRows {
    segments: [Option<CapSegment>; 3],
}

impl CapRows {
    fn push(&mut self, segment: CapSegment) {
        if matches!(segment, CapSegment::Children { count: 0, .. }) {
            return;
        }
        let slot = self
            .segments
            .iter_mut()
            .find(|candidate| candidate.is_none())
            .expect("a cap has at most three segments");
        *slot = Some(segment);
    }

    pub(super) fn row_count(self) -> usize {
        self.segments
            .iter()
            .flatten()
            .fold(0usize, |rows, segment| {
                rows.saturating_add(match segment {
                    CapSegment::Children { count, .. } => *count,
                    CapSegment::Marker { .. } => 1,
                })
            })
    }

    pub(super) fn row_at(self, output_row: usize) -> Option<CapRow> {
        let mut base = 0usize;
        for segment in self.segments.into_iter().flatten() {
            match segment {
                CapSegment::Children { start, count } => {
                    let end = base.saturating_add(count);
                    if output_row >= base && output_row < end {
                        return Some(CapRow::Child(
                            start.saturating_add(output_row.saturating_sub(base)),
                        ));
                    }
                    base = end;
                }
                CapSegment::Marker {
                    skipped,
                    kept,
                    total,
                    direction,
                    ..
                } => {
                    if output_row == base {
                        return Some(CapRow::Marker {
                            skipped,
                            kept,
                            total,
                            direction,
                        });
                    }
                    base = base.saturating_add(1);
                }
            }
        }
        None
    }

    pub(super) fn omitted_range(self) -> Option<(usize, usize)> {
        self.segments
            .iter()
            .flatten()
            .find_map(|segment| match segment {
                CapSegment::Marker {
                    omitted: Some(range),
                    ..
                } => Some(*range),
                _ => None,
            })
    }

    pub(super) fn remove_omitted_marker(&mut self) {
        for segment in &mut self.segments {
            if matches!(
                segment,
                Some(CapSegment::Marker {
                    omitted: Some(_),
                    ..
                })
            ) {
                *segment = None;
            }
        }
    }
}

pub(super) fn cap_rows(
    child_rows: usize,
    spec: &smelt_core::content::block_layout::CapSpec,
) -> CapRows {
    use smelt_core::content::block_layout::{CapKeep, CapMarker};

    let cap_rows = usize::from(spec.rows);
    let truncated = child_rows > cap_rows;
    let mut rows = CapRows::default();
    match spec.keep {
        CapKeep::Head { marker } => {
            let kept = child_rows.min(cap_rows);
            if truncated && marker == Some(CapMarker::Above) {
                rows.push(CapSegment::Marker {
                    skipped: child_rows.saturating_sub(kept),
                    kept,
                    total: None,
                    direction: "above",
                    omitted: None,
                });
            }
            rows.push(CapSegment::Children {
                start: 0,
                count: kept,
            });
            if truncated && marker == Some(CapMarker::Below) {
                rows.push(CapSegment::Marker {
                    skipped: child_rows.saturating_sub(kept),
                    kept,
                    total: None,
                    direction: "below",
                    omitted: None,
                });
            }
        }
        CapKeep::Tail { marker } => {
            let kept = child_rows.min(cap_rows);
            if truncated && marker == Some(CapMarker::Above) {
                rows.push(CapSegment::Marker {
                    skipped: child_rows.saturating_sub(kept),
                    kept,
                    total: spec.total_rows.filter(|total| *total > kept as u64),
                    direction: "above",
                    omitted: None,
                });
            }
            rows.push(CapSegment::Children {
                start: child_rows.saturating_sub(kept),
                count: kept,
            });
            if truncated && marker == Some(CapMarker::Below) {
                rows.push(CapSegment::Marker {
                    skipped: child_rows.saturating_sub(kept),
                    kept,
                    total: None,
                    direction: "below",
                    omitted: None,
                });
            }
        }
        CapKeep::HeadTail { head, marker } => {
            if !truncated {
                rows.push(CapSegment::Children {
                    start: 0,
                    count: child_rows,
                });
            } else {
                let head_rows = usize::from(head).min(cap_rows);
                let tail_rows = cap_rows.saturating_sub(head_rows);
                rows.push(CapSegment::Children {
                    start: 0,
                    count: head_rows,
                });
                if marker {
                    rows.push(CapSegment::Marker {
                        skipped: child_rows.saturating_sub(cap_rows),
                        kept: cap_rows,
                        total: None,
                        direction: "omitted",
                        omitted: Some((head_rows, child_rows.saturating_sub(tail_rows))),
                    });
                }
                rows.push(CapSegment::Children {
                    start: child_rows.saturating_sub(tail_rows),
                    count: tail_rows,
                });
            }
        }
    }
    rows
}
