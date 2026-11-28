use std::time::{Duration, Instant};

use crate::compositor::WindowInfo;

const SETTLE: Duration = Duration::from_millis(450);
const POINTER_SLOP: i32 = 32;

#[derive(Clone, Copy)]
struct Sample {
    position: [i32; 2],
    size: [u32; 2],
    cursor: [i32; 2],
    time: Instant,
}

impl Sample {
    fn new(window: &WindowInfo, cursor: [i32; 2], time: Instant) -> Self {
        Self {
            position: window.pos,
            size: window.size,
            cursor,
            time,
        }
    }

    fn cursor_on_window(self) -> bool {
        (0..2).all(|axis| {
            let offset = self.cursor[axis] - self.position[axis];
            offset >= -POINTER_SLOP && offset < self.size[axis] as i32 + POINTER_SLOP
        })
    }

    fn follows_pointer(self, next: Self) -> bool {
        // Hyprland also moves windows for layout changes and workspace commands.
        // Require matching pointer motion while the cursor is still on the app.
        let motion = delta(next.cursor, self.cursor);
        self.size == next.size
            && next.time.duration_since(self.time) <= SETTLE
            && self.cursor_on_window()
            && next.cursor_on_window()
            && (0..2).any(|axis| motion[axis].unsigned_abs() >= 4)
            && (0..2).all(|axis| {
                (next.position[axis] - self.position[axis]).abs_diff(motion[axis])
                    <= POINTER_SLOP as u32
            })
    }
}

struct Motion {
    start: [i32; 2],
    last_motion: Instant,
}

#[derive(Default)]
pub(super) struct WindowFollow {
    previous: Option<Sample>,
    motion: Option<Motion>,
}

pub(super) enum Movement {
    Idle,
    Moving,
    Settled([i32; 2]),
}

impl WindowFollow {
    pub fn observe(
        &mut self,
        window: &WindowInfo,
        cursor: [i32; 2],
        now: Instant,
    ) -> Movement {
        let current = Sample::new(window, cursor, now);
        let Some(previous) = self.previous.replace(current) else {
            return Movement::Idle;
        };
        if current.size != previous.size || !current.cursor_on_window() {
            self.motion = None;
        } else if current.position != previous.position {
            if previous.follows_pointer(current) {
                self.motion
                    .get_or_insert(Motion {
                        start: previous.position,
                        last_motion: now,
                    })
                    .last_motion = now;
            } else {
                self.motion = None;
            }
        } else if current.cursor != previous.cursor
            && let Some(motion) = &mut self.motion
        {
            motion.last_motion = now;
        }

        let Some(motion) = &self.motion else {
            return Movement::Idle;
        };
        if now.duration_since(motion.last_motion) >= SETTLE {
            let movement = delta(current.position, motion.start);
            self.motion = None;
            if movement == [0, 0] {
                Movement::Idle
            } else {
                Movement::Settled(movement)
            }
        } else {
            Movement::Moving
        }
    }

    pub fn crossed(&self, window: &WindowInfo, cursor: [i32; 2], now: Instant) -> Option<[i32; 2]> {
        let previous = self.previous?;
        let current = Sample::new(window, cursor, now);
        let recent_move = self.motion.as_ref().filter(|motion| {
            now.duration_since(motion.last_motion) <= SETTLE
                && previous.size == current.size
                && current.cursor_on_window()
        });
        if !previous.follows_pointer(current) && recent_move.is_none() {
            return None;
        }
        let movement = delta(
            current.position,
            recent_move.map_or(previous.position, |motion| motion.start),
        );
        (movement != [0, 0]).then_some(movement)
    }
}

fn delta(to: [i32; 2], from: [i32; 2]) -> [i32; 2] {
    [to[0] - from[0], to[1] - from[1]]
}
