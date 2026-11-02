// The `hover` step's mouse path, shared by the recorder that moves the mouse and the trailer's stage
// that draws the pointer back over the recording.

/** The cell under the mouse `ms` into a hover on a `cols` by `rows` screen: a figure eight over the
 * middle of the screen. */
export function hoverAt(ms: number, cols: number, rows: number): [number, number] {
  const k = ms / 1000;
  return [Math.round(cols / 2 + cols * 0.36 * Math.sin(k * 1.3)), Math.round(rows * 0.42 + rows * 0.3 * Math.sin(k * 2.6 + 0.6))];
}
