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
    cell_width: Pixels,
    cell_height: Pixels,
    bold: bool,
) -> Option<Vec<Path<Pixels>>> {
    let bounds = snap_bounds(bounds);
    if let Some(path) = block_element_path(ch, bounds) {
        return Some(vec![path]);
    }

    let glyph = glyph_for(ch)?;
    let cell = bounds.size;
    let origin = bounds.origin;
    let light_width = line_width(cell_width, bold);
    let heavy_width = heavy_line_width(light_width);
    let double_offset = light_width;
    let left = origin.x;
    let top = origin.y;
    let right = left + cell.width;
    let bottom = top + cell.height;
    let center_x = aligned_center(left, cell.width, light_width);
    let center_y = aligned_center(top, cell.height, light_width);
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
                (cell_height / 14.).max(px(0.5))
            } else {
                (cell_height / 26.).max(px(0.5))
            }
        } else {
            (cell_width / 20.).max(px(0.5))
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

        // Draw equal opposite single strokes straight through the cell before
        // shaping the remaining junction.
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
                    builder.line_to(point(center_x, bottom));
                } else {
                    builder.move_to(point(left, center_y));
                    builder.line_to(point(right, center_y));
                }
                arms[first] = NONE;
                arms[opposite] = NONE;
            }
        }

        // Normalize the remaining arms by their Konsole line-type ordering:
        // none, double, light, heavy. Rotating the maximum packed pattern
        // gives one set of junction shapes for every glyph orientation.
        let rotation = canonical_rotation(&arms);
        let pattern: [u8; 4] = std::array::from_fn(|offset| arms[(rotation + offset) % 4]);
        let mut directions: [usize; 4] = std::array::from_fn(|offset| (rotation + offset) % 4);
        let origins = [
            (center_x, top),
            (right, center_y),
            (center_x, bottom),
            (left, center_y),
        ];
        let center = (center_x, center_y);
        let half_light = light_width / 2.;

        let mut emit = |weight: u8, points: &[(Pixels, Pixels)]| {
            let builder = match weight {
                HEAVY => &mut heavy,
                DOUBLE => &mut double,
                _ => &mut light,
            };
            let Some(&(x, y)) = points.first() else {
                return;
            };
            builder.move_to(point(x, y));
            for &(x, y) in &points[1..] {
                builder.line_to(point(x, y));
            }
            match weight {
                HEAVY => has_heavy = true,
                DOUBLE => has_double = true,
                _ => has_light = true,
            }
        };

        let elbow = |top_direction: usize, right_direction: usize| {
            [origins[top_direction], center, origins[right_direction]]
        };
        let double_elbow = |top_direction: usize, right_direction: usize| {
            let top_vector = direction_vector(top_direction);
            let right_vector = direction_vector(right_direction);
            [
                offset_point(origins[top_direction], right_vector, double_offset),
                offset_point(
                    center,
                    (top_vector.0 + right_vector.0, top_vector.1 + right_vector.1),
                    double_offset,
                ),
                offset_point(origins[right_direction], top_vector, double_offset),
            ]
        };

        match pattern {
            [HEAVY, NONE, LIGHT, NONE] => {
                let branch_end = offset_point(center, direction_vector(directions[0]), half_light);
                emit(LIGHT, &[origins[directions[2]], branch_end]);
                emit(HEAVY, &[origins[directions[0]], center]);
            }
            [HEAVY, NONE, NONE, NONE] | [LIGHT, NONE, NONE, NONE] => {
                emit(pattern[0], &[origins[directions[0]], center]);
            }
            [HEAVY, HEAVY, LIGHT, LIGHT] => {
                emit(arms[directions[2]], &elbow(directions[2], directions[3]));
                emit(arms[directions[0]], &elbow(directions[0], directions[1]));
            }
            [HEAVY, HEAVY, NONE, NONE] | [LIGHT, LIGHT, NONE, NONE] => {
                emit(arms[directions[0]], &elbow(directions[0], directions[1]));
            }
            [HEAVY, LIGHT, NONE, NONE] | [HEAVY, NONE, NONE, LIGHT] => {
                if pattern[1] != NONE {
                    directions.swap(1, 3);
                }
                let heavy_end = offset_point(center, direction_vector(directions[2]), half_light);
                emit(LIGHT, &[origins[directions[3]], center]);
                emit(HEAVY, &[origins[directions[0]], heavy_end]);
            }
            [LIGHT, DOUBLE, NONE, NONE] | [LIGHT, NONE, NONE, DOUBLE] => {
                if pattern[1] != NONE {
                    directions.swap(1, 3);
                }
                let top = directions[0];
                let left_arm = directions[3];
                let bottom_vector = direction_vector(directions[2]);
                emit(
                    LIGHT,
                    &[
                        origins[top],
                        offset_point(center, bottom_vector, double_offset),
                        offset_point(origins[left_arm], bottom_vector, double_offset),
                    ],
                );
                emit(
                    DOUBLE,
                    &[
                        offset_point(origins[left_arm], bottom_vector, -double_offset),
                        offset_point(center, bottom_vector, -double_offset),
                    ],
                );
            }
            [HEAVY, HEAVY, LIGHT, NONE] | [HEAVY, HEAVY, NONE, LIGHT] => {
                if pattern[2] != NONE {
                    directions.swap(3, 2);
                    directions.swap(1, 0);
                }
                emit(arms[directions[0]], &elbow(directions[0], directions[1]));
                emit(LIGHT, &[origins[directions[3]], center]);
            }
            [HEAVY, LIGHT, LIGHT, NONE] | [HEAVY, NONE, LIGHT, LIGHT] => {
                if pattern[1] != NONE {
                    directions.swap(1, 3);
                }
                let heavy_end = offset_point(center, direction_vector(directions[2]), half_light);
                emit(HEAVY, &[origins[directions[0]], heavy_end]);
                emit(LIGHT, &elbow(directions[2], directions[3]));
            }
            [LIGHT, DOUBLE, NONE, DOUBLE] | [DOUBLE, NONE, DOUBLE, NONE] => {
                if pattern[0] == LIGHT {
                    let stem_end =
                        offset_point(center, direction_vector(directions[2]), -double_offset);
                    emit(LIGHT, &[origins[directions[0]], stem_end]);
                    directions.swap(1, 0);
                    directions.swap(3, 2);
                }
                let left_vector = direction_vector(directions[3]);
                let right_vector = direction_vector(directions[1]);
                emit(
                    DOUBLE,
                    &[
                        offset_point(origins[directions[0]], left_vector, double_offset),
                        offset_point(origins[directions[2]], left_vector, double_offset),
                    ],
                );
                emit(
                    DOUBLE,
                    &[
                        offset_point(origins[directions[0]], right_vector, double_offset),
                        offset_point(origins[directions[2]], right_vector, double_offset),
                    ],
                );
            }
            [DOUBLE, NONE, NONE, NONE] => {
                let left_vector = direction_vector(directions[3]);
                let right_vector = direction_vector(directions[1]);
                emit(
                    DOUBLE,
                    &[
                        offset_point(origins[directions[0]], left_vector, double_offset),
                        offset_point(center, left_vector, double_offset),
                    ],
                );
                emit(
                    DOUBLE,
                    &[
                        offset_point(origins[directions[0]], right_vector, double_offset),
                        offset_point(center, right_vector, double_offset),
                    ],
                );
            }
            [DOUBLE, DOUBLE, DOUBLE, DOUBLE] => {
                for (top_slot, right_slot) in [(0, 1), (2, 1), (0, 3), (2, 3)] {
                    emit(
                        DOUBLE,
                        &double_elbow(directions[top_slot], directions[right_slot]),
                    );
                }
            }
            [DOUBLE, DOUBLE, DOUBLE, NONE] => {
                let left_vector = direction_vector(directions[3]);
                emit(
                    DOUBLE,
                    &[
                        offset_point(origins[directions[0]], left_vector, double_offset),
                        offset_point(origins[directions[2]], left_vector, double_offset),
                    ],
                );
                emit(DOUBLE, &double_elbow(directions[0], directions[1]));
                emit(DOUBLE, &double_elbow(directions[2], directions[1]));
            }
            [DOUBLE, DOUBLE, NONE, NONE] => {
                let left_vector = direction_vector(directions[3]);
                let bottom_vector = direction_vector(directions[2]);
                emit(
                    DOUBLE,
                    &[
                        offset_point(origins[directions[0]], left_vector, double_offset),
                        offset_point(
                            center,
                            (
                                left_vector.0 + bottom_vector.0,
                                left_vector.1 + bottom_vector.1,
                            ),
                            double_offset,
                        ),
                        offset_point(origins[directions[1]], bottom_vector, double_offset),
                    ],
                );
                emit(DOUBLE, &double_elbow(directions[0], directions[1]));
            }
            _ => {
                // Preserve the directional-arm fallback for any future glyph
                // data that is not one of the canonical junction families.
                for (direction, weight) in arms.iter().copied().enumerate() {
                    match weight {
                        LIGHT | HEAVY => {
                            emit(weight, &[center, origins[direction]]);
                        }
                        DOUBLE => {
                            let vector = direction_vector((direction + 1) % 4);
                            emit(
                                DOUBLE,
                                &[
                                    offset_point(origins[direction], vector, double_offset),
                                    offset_point(center, vector, double_offset),
                                ],
                            );
                            emit(
                                DOUBLE,
                                &[
                                    offset_point(origins[direction], vector, -double_offset),
                                    offset_point(center, vector, -double_offset),
                                ],
                            );
                        }
                        _ => {}
                    }
                }
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
        if has_path && let Ok(path) = builder.build() {
            out.push(path);
        }
    }

    (!out.is_empty()).then_some(out)
}

pub(crate) fn is_block_element(ch: char) -> bool {
    (0x2580..=0x259f).contains(&u32::from(ch))
}

fn block_element_path(ch: char, bounds: Bounds<Pixels>) -> Option<Path<Pixels>> {
    let code = u32::from(ch);
    if !is_block_element(ch) {
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
            for (y, row) in pattern.iter().enumerate() {
                for x in 0..4 {
                    if *row & (1 << x) != 0 {
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

fn snap_pixel(value: Pixels) -> Pixels {
    px(f32::from(value).round())
}

fn snap_bounds(bounds: Bounds<Pixels>) -> Bounds<Pixels> {
    let left = snap_pixel(bounds.origin.x);
    let top = snap_pixel(bounds.origin.y);
    let right = snap_pixel(bounds.origin.x + bounds.size.width);
    let bottom = snap_pixel(bounds.origin.y + bounds.size.height);
    Bounds::from_corners(point(left, top), point(right, bottom))
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

fn offset_point(
    (x, y): (Pixels, Pixels),
    (dx, dy): (f32, f32),
    distance: Pixels,
) -> (Pixels, Pixels) {
    (x + distance * dx, y + distance * dy)
}

fn canonical_rotation(arms: &[u8; 4]) -> usize {
    let mut best_rotation = 0;
    let mut best_pattern = 0_u16;
    for rotation in 0..4 {
        let mut pattern = 0_u16;
        for offset in 0..4 {
            let weight = arms[(rotation + offset) % 4];
            let line_type = match weight {
                DOUBLE => 1,
                LIGHT => 2,
                HEAVY => 3,
                _ => 0,
            };
            pattern = (pattern << 2) | line_type;
        }
        if pattern > best_pattern {
            best_pattern = pattern;
            best_rotation = rotation;
        }
    }
    best_rotation
}
