//! A 3×5-pixel font drawn with half blocks: three terminal rows per line of text. Used to
//! show the pairing code large enough to read across a room.

/// Glyph rows, `#` = on. Most are 3 wide; M, N, W need more.
fn glyph(c: char) -> Option<[&'static str; 5]> {
    Some(match c.to_ascii_uppercase() {
        'A' => [".#.", "#.#", "###", "#.#", "#.#"],
        'B' => ["##.", "#.#", "##.", "#.#", "##."],
        'C' => [".##", "#..", "#..", "#..", ".##"],
        'D' => ["##.", "#.#", "#.#", "#.#", "##."],
        'E' => ["###", "#..", "##.", "#..", "###"],
        'F' => ["###", "#..", "##.", "#..", "#.."],
        'G' => [".##", "#..", "#.#", "#.#", ".##"],
        'H' => ["#.#", "#.#", "###", "#.#", "#.#"],
        'I' => ["###", ".#.", ".#.", ".#.", "###"],
        'J' => ["..#", "..#", "..#", "#.#", ".#."],
        'K' => ["#.#", "#.#", "##.", "#.#", "#.#"],
        'L' => ["#..", "#..", "#..", "#..", "###"],
        'M' => ["#...#", "##.##", "#.#.#", "#...#", "#...#"],
        'N' => ["#..#", "##.#", "#.##", "#..#", "#..#"],
        'O' => [".#.", "#.#", "#.#", "#.#", ".#."],
        'P' => ["##.", "#.#", "##.", "#..", "#.."],
        'Q' => [".#.", "#.#", "#.#", "##.", ".##"],
        'R' => ["##.", "#.#", "##.", "#.#", "#.#"],
        'S' => [".##", "#..", ".#.", "..#", "##."],
        'T' => ["###", ".#.", ".#.", ".#.", ".#."],
        'U' => ["#.#", "#.#", "#.#", "#.#", "###"],
        'V' => ["#.#", "#.#", "#.#", "#.#", ".#."],
        'W' => ["#...#", "#...#", "#.#.#", "##.##", "#...#"],
        'X' => ["#.#", "#.#", ".#.", "#.#", "#.#"],
        'Y' => ["#.#", "#.#", ".#.", ".#.", ".#."],
        'Z' => ["###", "..#", ".#.", "#..", "###"],
        '0' => ["###", "#.#", "#.#", "#.#", "###"],
        '1' => [".#.", "##.", ".#.", ".#.", "###"],
        '2' => ["##.", "..#", ".#.", "#..", "###"],
        '3' => ["##.", "..#", ".#.", "..#", "##."],
        '4' => ["#.#", "#.#", "###", "..#", "..#"],
        '5' => ["###", "#..", "##.", "..#", "##."],
        '6' => [".##", "#..", "###", "#.#", "###"],
        '7' => ["###", "..#", ".#.", ".#.", ".#."],
        '8' => ["###", "#.#", "###", "#.#", "###"],
        '9' => ["###", "#.#", "###", "..#", "##."],
        '-' => ["..", "..", "##", "..", ".."],
        ' ' => ["..", "..", "..", "..", ".."],
        '.' | '·' => [".", ".", "#", ".", "."],
        _ => return None,
    })
}

/// Columns `text` takes (one blank column between glyphs); `None` if a character has
/// no glyph.
#[cfg(test)]
pub fn width(text: &str) -> Option<usize> {
    let mut w = 0;
    for (i, c) in text.chars().enumerate() {
        w += glyph(c)?[0].len() + usize::from(i > 0);
    }
    Some(w)
}

/// Three rows of cells (`half` = upper, lower, full block) for `text`, or `None`.
pub fn render(text: &str, half: [char; 3]) -> Option<[Vec<char>; 3]> {
    let mut px: [Vec<bool>; 6] = Default::default();
    for (i, c) in text.chars().enumerate() {
        let g = glyph(c)?;
        for (r, row) in px.iter_mut().enumerate() {
            if i > 0 {
                row.push(false);
            }
            match g.get(r) {
                Some(line) => row.extend(line.chars().map(|b| b == '#')),
                None => row.extend(std::iter::repeat_n(false, g[0].len())),
            }
        }
    }
    let mut out: [Vec<char>; 3] = Default::default();
    for (k, line) in out.iter_mut().enumerate() {
        *line = (0..px[0].len())
            .map(|x| match (px[2 * k][x], px[2 * k + 1][x]) {
                (true, true) => half[2],
                (true, false) => half[0],
                (false, true) => half[1],
                _ => ' ',
            })
            .collect();
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_code() {
        let r = render("41-ab", ['▀', '▄', '█']).unwrap_or_default();
        assert_eq!(r[0].len(), width("41-ab").unwrap_or(0));
        let top: String = r[0].iter().collect();
        assert!(top.starts_with("█ █"), "{top}");
        assert!(render("é", ['a', 'b', 'c']).is_none());
        // Every letter and digit has a glyph.
        assert!(width("abcdefghijklmnopqrstuvwxyz0123456789-").is_some());
    }
}
