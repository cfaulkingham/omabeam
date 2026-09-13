use crate::hypr::{Client, Monitor, Rect};

#[derive(Debug, Clone, PartialEq)]
pub struct RelativeRect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Tile {
    pub index: usize,
    pub rect: RelativeRect,
}

pub fn tiles_for(clients: &[&Client], monitor: &Monitor) -> Vec<Tile> {
    let canvas = canvas_for(clients, monitor);
    clients
        .iter()
        .enumerate()
        .map(|(index, client)| Tile {
            index,
            rect: relative_rect(&client.rect(), &canvas),
        })
        .collect()
}

pub fn nearest_in_direction(current: usize, tiles: &[Tile], dx: i32, dy: i32) -> Option<usize> {
    if tiles.is_empty() {
        return None;
    }
    let origin = tiles.get(current)?.rect.center();
    let mut best: Option<(f32, usize)> = None;
    for tile in tiles {
        if tile.index == current {
            continue;
        }
        let center = tile.rect.center();
        let move_x = center.0 - origin.0;
        let move_y = center.1 - origin.1;
        if dx != 0 && move_x * dx as f32 <= 0.002 {
            continue;
        }
        if dy != 0 && move_y * dy as f32 <= 0.002 {
            continue;
        }
        let along = if dx != 0 { move_x.abs() } else { move_y.abs() };
        let sideways = if dx != 0 { move_y.abs() } else { move_x.abs() };
        let score = along + sideways * 2.0;
        if best
            .map(|(best_score, _)| score < best_score)
            .unwrap_or(true)
        {
            best = Some((score, tile.index));
        }
    }
    best.map(|(_, index)| index)
}

impl RelativeRect {
    fn center(&self) -> (f32, f32) {
        (self.x + self.w / 2.0, self.y + self.h / 2.0)
    }
}

fn canvas_for(clients: &[&Client], monitor: &Monitor) -> Rect {
    let mut canvas = monitor.canvas();
    for client in clients {
        let rect = client.rect();
        let right = rect.x + rect.w;
        let bottom = rect.y + rect.h;
        if rect.x < canvas.x {
            canvas.w += canvas.x - rect.x;
            canvas.x = rect.x;
        }
        if rect.y < canvas.y {
            canvas.h += canvas.y - rect.y;
            canvas.y = rect.y;
        }
        if right > canvas.x + canvas.w {
            canvas.w = right - canvas.x;
        }
        if bottom > canvas.y + canvas.h {
            canvas.h = bottom - canvas.y;
        }
    }
    canvas
}

fn relative_rect(rect: &Rect, canvas: &Rect) -> RelativeRect {
    let width = canvas.w.max(1) as f32;
    let height = canvas.h.max(1) as f32;
    let mut x = ((rect.x - canvas.x) as f32 / width).clamp(0.0, 0.98);
    let mut y = ((rect.y - canvas.y) as f32 / height).clamp(0.0, 0.98);
    let mut w = (rect.w as f32 / width).clamp(0.02, 1.0);
    let mut h = (rect.h as f32 / height).clamp(0.02, 1.0);
    if x + w > 1.0 {
        w = (1.0 - x).max(0.02);
        x = (1.0 - w).clamp(0.0, 0.98);
    }
    if y + h > 1.0 {
        h = (1.0 - y).max(0.02);
        y = (1.0 - h).clamp(0.0, 0.98);
    }
    RelativeRect { x, y, w, h }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hypr::{Client, Workspace};

    fn client(x: i32, y: i32, w: i32, h: i32) -> Client {
        Client {
            address: "0x1".into(),
            class: "foot".into(),
            title: "term".into(),
            workspace: Workspace {
                id: 1,
                name: "1".into(),
            },
            monitor: 0,
            floating: false,
            mapped: true,
            hidden: false,
            at: [x, y],
            size: [w, h],
            focus_history_id: 0,
            portal_id: None,
            stable_id: String::new(),
        }
    }

    fn monitor() -> Monitor {
        Monitor {
            transform: 0,
            id: 0,
            name: "HDMI-A-1".into(),
            description: String::new(),
            width: 1920,
            height: 1080,
            x: 0,
            y: 0,
            scale: 1.25,
            refresh_rate: 60.,
            focused: true,
            reserved: [0, 26, 0, 0],
            active_workspace: Workspace {
                id: 1,
                name: "1".into(),
            },
            special_workspace: Workspace {
                id: 0,
                name: String::new(),
            },
        }
    }

    #[test]
    fn maps_side_by_side_tiles() {
        let left = client(12, 38, 749, 814);
        let right = client(775, 38, 749, 814);
        let refs = vec![&left, &right];
        let tiles = tiles_for(&refs, &monitor());
        assert_eq!(tiles.len(), 2);
        assert!(tiles[0].rect.x < tiles[1].rect.x);
        assert!(tiles[0].rect.w > 0.3);
        assert!(tiles[1].rect.x > 0.4);
        for tile in &tiles {
            assert!(tile.rect.x + tile.rect.w <= 1.001);
            assert!(tile.rect.y + tile.rect.h <= 1.001);
        }
    }

    #[test]
    fn clamps_overflowing_windows_inside_the_map() {
        let huge = client(1400, 800, 800, 400);
        let refs = vec![&huge];
        let tiles = tiles_for(&refs, &monitor());
        let rect = &tiles[0].rect;
        assert!(rect.x >= 0.0 && rect.y >= 0.0);
        assert!(rect.x + rect.w <= 1.001);
        assert!(rect.y + rect.h <= 1.001);
    }

    #[test]
    fn finds_right_neighbor() {
        let tiles = vec![
            Tile {
                index: 0,
                rect: RelativeRect {
                    x: 0.0,
                    y: 0.0,
                    w: 0.5,
                    h: 1.0,
                },
            },
            Tile {
                index: 1,
                rect: RelativeRect {
                    x: 0.5,
                    y: 0.0,
                    w: 0.5,
                    h: 1.0,
                },
            },
        ];
        assert_eq!(nearest_in_direction(0, &tiles, 1, 0), Some(1));
        assert_eq!(nearest_in_direction(1, &tiles, -1, 0), Some(0));
        assert_eq!(nearest_in_direction(0, &tiles, 0, 1), None);
    }
}
