pub type RowIndex = u64;

pub fn row_to_usize(row: RowIndex) -> usize {
    row.min(usize::MAX as RowIndex) as usize
}
