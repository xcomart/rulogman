//! Cell geometry for the Unicode Box Drawing block.

use gpui::{Bounds, Path, PathBuilder, Pixels, point, px};

const NONE: u8 = 0;
const LIGHT: u8 = 1;
const HEAVY: u8 = 2;
const DOUBLE: u8 = 3;

#[derive(Clone, Copy)]
struct Glyph {
    /// North, east, south, west; each arm carries its own line weight.
    arms: [u8; 4],
    /// Number of evenly spaced dashes for straight horizontal/vertical lines.
    dash_count: u8,
    /// Rounded corner: northwest, northeast, southeast, southwest.
    arc: u8,
    /// Diagonal bits: `/` = 1, `\\` = 2.
    diagonal: u8,
}

const fn glyph(n: u8, e: u8, s: u8, w: u8) -> Glyph {
    Glyph {
        arms: [n, e, s, w],
        dash_count: 0,
        arc: 0,
        diagonal: 0,
    }
}

const fn dashed(vertical: bool, weight: u8, count: u8) -> Glyph {
    if vertical {
        Glyph {
            arms: [weight, NONE, weight, NONE],
            dash_count: count,
            arc: 0,
            diagonal: 0,
        }
    } else {
        Glyph {
            arms: [NONE, weight, NONE, weight],
            dash_count: count,
            arc: 0,
            diagonal: 0,
        }
    }
}

const fn arc(corner: u8) -> Glyph {
    Glyph {
        arms: [NONE; 4],
        dash_count: 0,
        arc: corner,
        diagonal: 0,
    }
}

const fn diagonal(bits: u8) -> Glyph {
    Glyph {
        arms: [NONE; 4],
        dash_count: 0,
        arc: 0,
        diagonal: bits,
    }
}

// U+2500–U+257F, in code point order. Unicode names distinguish the weight
// of each directional arm; keeping that distinction here preserves mixed
// junctions as well as the light, heavy and double families.
const GLYPHS: [Glyph; 128] = [
    glyph(NONE, LIGHT, NONE, LIGHT),       // 2500 ─
    glyph(NONE, HEAVY, NONE, HEAVY),       // 2501 ━
    glyph(LIGHT, NONE, LIGHT, NONE),       // 2502 │
    glyph(HEAVY, NONE, HEAVY, NONE),       // 2503 ┃
    dashed(false, LIGHT, 3),               // 2504 ┄
    dashed(false, HEAVY, 3),               // 2505 ┅
    dashed(true, LIGHT, 3),                // 2506 ┆
    dashed(true, HEAVY, 3),                // 2507 ┇
    dashed(false, LIGHT, 4),               // 2508 ┈
    dashed(false, HEAVY, 4),               // 2509 ┉
    dashed(true, LIGHT, 4),                // 250A ┊
    dashed(true, HEAVY, 4),                // 250B ┋
    glyph(NONE, LIGHT, LIGHT, NONE),       // 250C ┌
    glyph(NONE, HEAVY, LIGHT, NONE),       // 250D ┍
    glyph(NONE, LIGHT, HEAVY, NONE),       // 250E ┎
    glyph(NONE, HEAVY, HEAVY, NONE),       // 250F ┏
    glyph(NONE, NONE, LIGHT, LIGHT),       // 2510 ┐
    glyph(NONE, NONE, LIGHT, HEAVY),       // 2511 ┑
    glyph(NONE, NONE, HEAVY, LIGHT),       // 2512 ┒
    glyph(NONE, NONE, HEAVY, HEAVY),       // 2513 ┓
    glyph(LIGHT, LIGHT, NONE, NONE),       // 2514 └
    glyph(LIGHT, HEAVY, NONE, NONE),       // 2515 ┕
    glyph(HEAVY, LIGHT, NONE, NONE),       // 2516 ┖
    glyph(HEAVY, HEAVY, NONE, NONE),       // 2517 ┗
    glyph(LIGHT, NONE, NONE, LIGHT),       // 2518 ┘
    glyph(LIGHT, NONE, NONE, HEAVY),       // 2519 ┙
    glyph(HEAVY, NONE, NONE, LIGHT),       // 251A ┚
    glyph(HEAVY, NONE, NONE, HEAVY),       // 251B ┛
    glyph(LIGHT, LIGHT, LIGHT, NONE),      // 251C ├
    glyph(LIGHT, HEAVY, LIGHT, NONE),      // 251D ┝
    glyph(HEAVY, LIGHT, LIGHT, NONE),      // 251E ┞
    glyph(LIGHT, LIGHT, HEAVY, NONE),      // 251F ┟
    glyph(HEAVY, LIGHT, HEAVY, NONE),      // 2520 ┠
    glyph(HEAVY, HEAVY, LIGHT, NONE),      // 2521 ┡
    glyph(LIGHT, HEAVY, HEAVY, NONE),      // 2522 ┢
    glyph(HEAVY, HEAVY, HEAVY, NONE),      // 2523 ┣
    glyph(LIGHT, NONE, LIGHT, LIGHT),      // 2524 ┤
    glyph(LIGHT, NONE, LIGHT, HEAVY),      // 2525 ┥
    glyph(HEAVY, NONE, LIGHT, LIGHT),      // 2526 ┦
    glyph(LIGHT, NONE, HEAVY, LIGHT),      // 2527 ┧
    glyph(HEAVY, NONE, HEAVY, LIGHT),      // 2528 ┨
    glyph(HEAVY, NONE, LIGHT, HEAVY),      // 2529 ┩
    glyph(LIGHT, NONE, HEAVY, HEAVY),      // 252A ┪
    glyph(HEAVY, NONE, HEAVY, HEAVY),      // 252B ┫
    glyph(NONE, LIGHT, LIGHT, LIGHT),      // 252C ┬
    glyph(NONE, LIGHT, LIGHT, HEAVY),      // 252D ┭
    glyph(NONE, HEAVY, LIGHT, LIGHT),      // 252E ┮
    glyph(NONE, HEAVY, LIGHT, HEAVY),      // 252F ┯
    glyph(NONE, LIGHT, HEAVY, LIGHT),      // 2530 ┰
    glyph(NONE, LIGHT, HEAVY, HEAVY),      // 2531 ┱
    glyph(NONE, HEAVY, HEAVY, LIGHT),      // 2532 ┲
    glyph(NONE, HEAVY, HEAVY, HEAVY),      // 2533 ┳
    glyph(LIGHT, LIGHT, NONE, LIGHT),      // 2534 ┴
    glyph(LIGHT, LIGHT, NONE, HEAVY),      // 2535 ┵
    glyph(LIGHT, HEAVY, NONE, LIGHT),      // 2536 ┶
    glyph(LIGHT, HEAVY, NONE, HEAVY),      // 2537 ┷
    glyph(HEAVY, LIGHT, NONE, LIGHT),      // 2538 ┸
    glyph(HEAVY, LIGHT, NONE, HEAVY),      // 2539 ┹
    glyph(HEAVY, HEAVY, NONE, LIGHT),      // 253A ┺
    glyph(HEAVY, HEAVY, NONE, HEAVY),      // 253B ┻
    glyph(LIGHT, LIGHT, LIGHT, LIGHT),     // 253C ┼
    glyph(LIGHT, LIGHT, LIGHT, HEAVY),     // 253D ┽
    glyph(LIGHT, HEAVY, LIGHT, LIGHT),     // 253E ┾
    glyph(LIGHT, HEAVY, LIGHT, HEAVY),     // 253F ┿
    glyph(HEAVY, LIGHT, LIGHT, LIGHT),     // 2540 ╀
    glyph(LIGHT, LIGHT, HEAVY, LIGHT),     // 2541 ╁
    glyph(HEAVY, LIGHT, HEAVY, LIGHT),     // 2542 ╂
    glyph(HEAVY, LIGHT, LIGHT, HEAVY),     // 2543 ╃
    glyph(HEAVY, HEAVY, LIGHT, LIGHT),     // 2544 ╄
    glyph(LIGHT, LIGHT, HEAVY, HEAVY),     // 2545 ╅
    glyph(LIGHT, HEAVY, HEAVY, LIGHT),     // 2546 ╆
    glyph(HEAVY, HEAVY, LIGHT, HEAVY),     // 2547 ╇
    glyph(LIGHT, HEAVY, HEAVY, HEAVY),     // 2548 ╈
    glyph(HEAVY, LIGHT, HEAVY, HEAVY),     // 2549 ╉
    glyph(HEAVY, HEAVY, HEAVY, LIGHT),     // 254A ╊
    glyph(HEAVY, HEAVY, HEAVY, HEAVY),     // 254B ╋
    dashed(false, LIGHT, 2),               // 254C ╌
    dashed(false, HEAVY, 2),               // 254D ╍
    dashed(true, LIGHT, 2),                // 254E ╎
    dashed(true, HEAVY, 2),                // 254F ╏
    glyph(NONE, DOUBLE, NONE, DOUBLE),     // 2550 ═
    glyph(DOUBLE, NONE, DOUBLE, NONE),     // 2551 ║
    glyph(NONE, DOUBLE, LIGHT, NONE),      // 2552 ╒
    glyph(NONE, LIGHT, DOUBLE, NONE),      // 2553 ╓
    glyph(NONE, DOUBLE, DOUBLE, NONE),     // 2554 ╔
    glyph(NONE, NONE, LIGHT, DOUBLE),      // 2555 ╕
    glyph(NONE, NONE, DOUBLE, LIGHT),      // 2556 ╖
    glyph(NONE, NONE, DOUBLE, DOUBLE),     // 2557 ╗
    glyph(LIGHT, DOUBLE, NONE, NONE),      // 2558 ╘
    glyph(DOUBLE, LIGHT, NONE, NONE),      // 2559 ╙
    glyph(DOUBLE, DOUBLE, NONE, NONE),     // 255A ╚
    glyph(LIGHT, NONE, NONE, DOUBLE),      // 255B ╛
    glyph(DOUBLE, NONE, NONE, LIGHT),      // 255C ╜
    glyph(DOUBLE, NONE, NONE, DOUBLE),     // 255D ╝
    glyph(LIGHT, DOUBLE, LIGHT, NONE),     // 255E ╞
    glyph(DOUBLE, LIGHT, DOUBLE, NONE),    // 255F ╟
    glyph(DOUBLE, DOUBLE, DOUBLE, NONE),   // 2560 ╠
    glyph(LIGHT, NONE, LIGHT, DOUBLE),     // 2561 ╡
    glyph(DOUBLE, NONE, DOUBLE, LIGHT),    // 2562 ╢
    glyph(DOUBLE, NONE, DOUBLE, DOUBLE),   // 2563 ╣
    glyph(NONE, DOUBLE, LIGHT, DOUBLE),    // 2564 ╤
    glyph(NONE, LIGHT, DOUBLE, LIGHT),     // 2565 ╥
    glyph(NONE, DOUBLE, DOUBLE, DOUBLE),   // 2566 ╦
    glyph(LIGHT, DOUBLE, NONE, DOUBLE),    // 2567 ╧
    glyph(DOUBLE, LIGHT, NONE, LIGHT),     // 2568 ╨
    glyph(DOUBLE, DOUBLE, NONE, DOUBLE),   // 2569 ╩
    glyph(LIGHT, DOUBLE, LIGHT, DOUBLE),   // 256A ╪
    glyph(DOUBLE, LIGHT, DOUBLE, LIGHT),   // 256B ╫
    glyph(DOUBLE, DOUBLE, DOUBLE, DOUBLE), // 256C ╬
    arc(1),                                // 256D ╭
    arc(2),                                // 256E ╮
    arc(3),                                // 256F ╯
    arc(4),                                // 2570 ╰
    diagonal(1),                           // 2571 ╱
    diagonal(2),                           // 2572 ╲
    diagonal(3),                           // 2573 ╳
    glyph(NONE, NONE, NONE, LIGHT),        // 2574 ╴
    glyph(LIGHT, NONE, NONE, NONE),        // 2575 ╵
    glyph(NONE, LIGHT, NONE, NONE),        // 2576 ╶
    glyph(NONE, NONE, LIGHT, NONE),        // 2577 ╷
    glyph(NONE, NONE, NONE, HEAVY),        // 2578 ╸
    glyph(HEAVY, NONE, NONE, NONE),        // 2579 ╹
    glyph(NONE, HEAVY, NONE, NONE),        // 257A ╺
    glyph(NONE, NONE, HEAVY, NONE),        // 257B ╻
    glyph(NONE, HEAVY, NONE, LIGHT),       // 257C ╼
    glyph(LIGHT, NONE, HEAVY, NONE),       // 257D ╽
    glyph(NONE, LIGHT, NONE, HEAVY),       // 257E ╾
    glyph(HEAVY, NONE, LIGHT, NONE),       // 257F ╿
];

pub(crate) fn paths(
    ch: char,
    bounds: Bounds<Pixels>,
    _font_size: Pixels,
    bold: bool,
) -> Option<Vec<Path<Pixels>>> {
    if let Some(path) = block_element_path(ch, bounds) {
        return Some(vec![path]);
    }

    let glyph = glyph_for(ch)?;
    let cell = bounds.size;
    let origin = bounds.origin;
    let light_width = line_width(cell.width, bold);
    let heavy_width = heavy_line_width(light_width);
    let double_offset = light_width;
    let left = origin.x;
    let top = origin.y;
    let right = left + cell.width;
    let bottom = top + cell.height;
    let center_x = aligned_center(left, cell.width, light_width);
    let center_y = aligned_center(top, cell.height, light_width);
    // Slightly carry south-facing solid strokes past the row boundary to
    // cover the antialiased join with the next row.
    let bottom_overlap = px(1.);
    let mut out = Vec::with_capacity(4);

    let mut light = PathBuilder::stroke(light_width);
    let mut has_light = false;
    let mut heavy = PathBuilder::stroke(heavy_width);
    let mut has_heavy = false;
    let mut double = PathBuilder::stroke(light_width);
    let mut has_double = false;

    if glyph.dash_count > 0 {
        let weight = if glyph.arms[0] == HEAVY || glyph.arms[1] == HEAVY {
            HEAVY
        } else {
            LIGHT
        };
        let thickness = if weight == HEAVY {
            heavy_width
        } else {
            light_width
        };
        let vertical = glyph.arms[0] != NONE;
        let span = if vertical { cell.height } else { cell.width };
        let count = f32::from(glyph.dash_count);
        let half_gap = if vertical {
            if glyph.dash_count == 2 {
                (cell.height / 14.).max(px(0.5))
            } else {
                (cell.height / 26.).max(px(0.5))
            }
        } else {
            (cell.width / 20.).max(px(0.5))
        };
        let mut builder = PathBuilder::stroke(thickness);
        for index in 0..glyph.dash_count {
            let start = span * f32::from(index) / count;
            let end = span * f32::from(index + 1) / count;
            if vertical {
                builder.move_to(point(center_x, top + start + half_gap));
                builder.line_to(point(center_x, top + end - half_gap));
            } else {
                builder.move_to(point(left + start + half_gap, center_y));
                builder.line_to(point(left + end - half_gap, center_y));
            }
        }
        if let Ok(path) = builder.build() {
            out.push(path);
        }
    } else {
        let mut arms = glyph.arms;

        // Keep matching straight strokes continuous across their cell. Any
        // remaining arms meet these trunks as branches of the same junction.
        for (first, opposite) in [(0, 2), (1, 3)] {
            let weight = arms[first];
            if weight != NONE && weight == arms[opposite] && weight != DOUBLE {
                let builder = if weight == HEAVY {
                    &mut heavy
                } else {
                    &mut light
                };
                if first == 0 {
                    builder.move_to(point(center_x, top));
                    builder.line_to(point(center_x, bottom + bottom_overlap));
                } else {
                    builder.move_to(point(left, center_y));
                    builder.line_to(point(right, center_y));
                }
                arms[first] = NONE;
                arms[opposite] = NONE;
            }
        }

        let mut junctions = [(center_x, center_y); 4];
        for (first, opposite) in [(0, 2), (1, 3)] {
            let (first_weight, opposite_weight) = (arms[first], arms[opposite]);
            if matches!(
                (first_weight, opposite_weight),
                (LIGHT, HEAVY) | (HEAVY, LIGHT)
            ) {
                let (heavy_direction, light_direction) = if first_weight == HEAVY {
                    (first, opposite)
                } else {
                    (opposite, first)
                };
                let (dx, dy) = direction_vector(heavy_direction);
                let inset = light_width / 2.;
                junctions[light_direction] = (center_x + inset * dx, center_y + inset * dy);
            }
        }

        for (direction, weight) in arms.iter().copied().enumerate() {
            if weight == NONE {
                continue;
            }
            let (junction_x, junction_y) = junctions[direction];
            match weight {
                LIGHT => {
                    add_arm(
                        &mut light,
                        direction,
                        left,
                        top,
                        right,
                        bottom,
                        junction_x,
                        junction_y,
                        bottom_overlap,
                    );
                    has_light = true;
                }
                HEAVY => {
                    add_arm(
                        &mut heavy,
                        direction,
                        left,
                        top,
                        right,
                        bottom,
                        junction_x,
                        junction_y,
                        bottom_overlap,
                    );
                    has_heavy = true;
                }
                DOUBLE => {
                    add_double_arm(
                        &mut double,
                        direction,
                        left,
                        top,
                        right,
                        bottom,
                        junction_x,
                        junction_y,
                        double_offset,
                        bottom_overlap,
                    );
                    has_double = true;
                }
                _ => return None,
            }
        }
    }

    if glyph.arc > 0 {
        let radius = cell.width * 3. / 8.;
        let curve = point(radius, radius);
        match glyph.arc {
            1 => {
                light.move_to(point(center_x, bottom));
                light.line_to(point(center_x, center_y + radius));
                light.arc_to(
                    curve,
                    px(0.),
                    false,
                    true,
                    point(center_x + radius, center_y),
                );
                light.line_to(point(right, center_y));
            }
            2 => {
                light.move_to(point(center_x, bottom));
                light.line_to(point(center_x, center_y + radius));
                light.arc_to(
                    curve,
                    px(0.),
                    false,
                    false,
                    point(center_x - radius, center_y),
                );
                light.line_to(point(left, center_y));
            }
            3 => {
                light.move_to(point(center_x, top));
                light.line_to(point(center_x, center_y - radius));
                light.arc_to(
                    curve,
                    px(0.),
                    false,
                    true,
                    point(center_x - radius, center_y),
                );
                light.line_to(point(left, center_y));
            }
            4 => {
                light.move_to(point(center_x, top));
                light.line_to(point(center_x, center_y - radius));
                light.arc_to(
                    curve,
                    px(0.),
                    false,
                    false,
                    point(center_x + radius, center_y),
                );
                light.line_to(point(right, center_y));
            }
            _ => return None,
        }
        has_light = true;
    }

    if glyph.diagonal & 1 != 0 {
        light.move_to(point(left, bottom));
        light.line_to(point(right, top));
        has_light = true;
    }
    if glyph.diagonal & 2 != 0 {
        light.move_to(point(left, top));
        light.line_to(point(right, bottom));
        has_light = true;
    }

    for (builder, has_path) in [(light, has_light), (heavy, has_heavy), (double, has_double)] {
        if has_path {
            if let Ok(path) = builder.build() {
                out.push(path);
            }
        }
    }

    (!out.is_empty()).then_some(out)
}

fn block_element_path(ch: char, bounds: Bounds<Pixels>) -> Option<Path<Pixels>> {
    let code = u32::from(ch);
    if !(0x2580..=0x259f).contains(&code) {
        return None;
    }

    let left = bounds.origin.x;
    let top = bounds.origin.y;
    let right = left + bounds.size.width;
    let bottom = top + bounds.size.height;
    let half_x = left + bounds.size.width / 2.;
    let half_y = top + bounds.size.height / 2.;
    let mut builder = PathBuilder::fill();

    match code {
        0x2580 => add_rect(&mut builder, left, top, right, half_y),
        0x2581..=0x2587 => {
            let eighths = (code - 0x2580) as f32;
            add_rect(
                &mut builder,
                left,
                bottom - bounds.size.height * (eighths / 8.),
                right,
                bottom,
            );
        }
        0x2588 => add_rect(&mut builder, left, top, right, bottom),
        0x2589..=0x258f => {
            let eighths = (0x2590 - code) as f32;
            add_rect(
                &mut builder,
                left,
                top,
                left + bounds.size.width * (eighths / 8.),
                bottom,
            );
        }
        0x2590 => add_rect(&mut builder, half_x, top, right, bottom),
        0x2591..=0x2593 => {
            let shade = code - 0x2590;
            let pattern: [u8; 4] = match shade {
                1 => [0b0101, 0, 0b0101, 0],
                2 => [0b0101, 0b1010, 0b0101, 0b1010],
                _ => [0b1111, 0b0101, 0b1111, 0b0101],
            };
            for y in 0..4 {
                for x in 0..4 {
                    if pattern[y] & (1 << x) != 0 {
                        let tile_width = bounds.size.width / 4.;
                        let tile_height = bounds.size.height / 4.;
                        add_rect(
                            &mut builder,
                            left + tile_width * x as f32,
                            top + tile_height * y as f32,
                            left + tile_width * (x + 1) as f32,
                            top + tile_height * (y + 1) as f32,
                        );
                    }
                }
            }
        }
        0x2594 => add_rect(
            &mut builder,
            left,
            top,
            right,
            top + bounds.size.height / 8.,
        ),
        0x2595 => add_rect(
            &mut builder,
            right - bounds.size.width / 8.,
            top,
            right,
            bottom,
        ),
        0x2596 => add_rect(&mut builder, left, half_y, half_x, bottom),
        0x2597 => add_rect(&mut builder, half_x, half_y, right, bottom),
        0x2598 => add_rect(&mut builder, left, top, half_x, half_y),
        0x2599 => {
            add_rect(&mut builder, left, top, half_x, half_y);
            add_rect(&mut builder, left, half_y, half_x, bottom);
            add_rect(&mut builder, half_x, half_y, right, bottom);
        }
        0x259a => {
            add_rect(&mut builder, left, top, half_x, half_y);
            add_rect(&mut builder, half_x, half_y, right, bottom);
        }
        0x259b => {
            add_rect(&mut builder, left, top, half_x, half_y);
            add_rect(&mut builder, half_x, top, right, half_y);
            add_rect(&mut builder, left, half_y, half_x, bottom);
        }
        0x259c => {
            add_rect(&mut builder, half_x, top, right, half_y);
            add_rect(&mut builder, half_x, half_y, right, bottom);
            add_rect(&mut builder, left, top, half_x, half_y);
        }
        0x259d => add_rect(&mut builder, half_x, top, right, half_y),
        0x259e => {
            add_rect(&mut builder, half_x, top, right, half_y);
            add_rect(&mut builder, left, half_y, half_x, bottom);
        }
        0x259f => {
            add_rect(&mut builder, half_x, top, right, half_y);
            add_rect(&mut builder, left, half_y, half_x, bottom);
            add_rect(&mut builder, half_x, half_y, right, bottom);
        }
        _ => return None,
    }

    builder.build().ok()
}

fn add_rect(builder: &mut PathBuilder, left: Pixels, top: Pixels, right: Pixels, bottom: Pixels) {
    builder.move_to(point(left, top));
    builder.line_to(point(right, top));
    builder.line_to(point(right, bottom));
    builder.line_to(point(left, bottom));
    builder.close();
}

fn glyph_for(ch: char) -> Option<Glyph> {
    let code = u32::from(ch);
    let index = usize::try_from(code.checked_sub(0x2500)?).ok()?;
    let glyph = *GLYPHS.get(index)?;
    (glyph.dash_count > 0 || glyph.arc > 0 || glyph.diagonal > 0 || glyph.arms != [NONE; 4])
        .then_some(glyph)
}

fn line_width(cell_width: Pixels, bold: bool) -> Pixels {
    let base_width = f32::from(cell_width) / 6.5;
    let bold_coefficient = if bold { 1.5 } else { 1.0 };
    let minimum = if bold && cell_width >= px(7.) {
        base_width + 1.0
    } else {
        1.0
    };
    px((base_width * bold_coefficient).max(minimum).round())
}

fn heavy_line_width(light_width: Pixels) -> Pixels {
    let half_extra = (f32::from(light_width) / 3.0).max(1.0).round();
    px(f32::from(light_width) + 2.0 * half_extra)
}

fn aligned_center(origin: Pixels, span: Pixels, line_width: Pixels) -> Pixels {
    let center = (f32::from(origin) + f32::from(span) / 2.0).floor();
    let half_pixel = if f32::from(line_width) as u32 % 2 == 1 {
        0.5
    } else {
        0.0
    };
    px(center + half_pixel)
}

fn direction_vector(direction: usize) -> (f32, f32) {
    match direction {
        0 => (0., -1.),
        1 => (1., 0.),
        2 => (0., 1.),
        _ => (-1., 0.),
    }
}

#[allow(clippy::too_many_arguments)]
fn add_arm(
    builder: &mut PathBuilder,
    direction: usize,
    left: Pixels,
    top: Pixels,
    right: Pixels,
    bottom: Pixels,
    junction_x: Pixels,
    junction_y: Pixels,
    bottom_overlap: Pixels,
) {
    let (start, end) = match direction {
        0 => (point(junction_x, junction_y), point(junction_x, top)),
        1 => (point(junction_x, junction_y), point(right, junction_y)),
        2 => (
            point(junction_x, junction_y),
            point(junction_x, bottom + bottom_overlap),
        ),
        _ => (point(junction_x, junction_y), point(left, junction_y)),
    };
    builder.move_to(start);
    builder.line_to(end);
}

#[allow(clippy::too_many_arguments)]
fn add_double_arm(
    builder: &mut PathBuilder,
    direction: usize,
    left: Pixels,
    top: Pixels,
    right: Pixels,
    bottom: Pixels,
    center_x: Pixels,
    center_y: Pixels,
    offset: Pixels,
    bottom_overlap: Pixels,
) {
    for side in [-1., 1.] {
        let offset = offset * side;
        let (start, end) = match direction {
            0 => (
                point(center_x + offset, center_y),
                point(center_x + offset, top),
            ),
            1 => (
                point(center_x, center_y + offset),
                point(right, center_y + offset),
            ),
            2 => (
                point(center_x + offset, center_y),
                point(center_x + offset, bottom + bottom_overlap),
            ),
            _ => (
                point(center_x, center_y + offset),
                point(left, center_y + offset),
            ),
        };
        builder.move_to(start);
        builder.line_to(end);
    }
}
