//! A Viewer's own zoom of the Monitor's picture: the mouse wheel zooms at the
//! pointer, a drag moves the zoomed picture. The same rules as the web page's
//! (`app.js` `zoomAround` / `clampZoomPan`), so every Tawny zooms alike.
//!
//! The picture is fitted whole into the stage (`contain`). Zoomed, the image
//! element is the stage scaled by `zoom` and placed at (`x`, `y`): a point p
//! of the unzoomed stage lands at `(x, y) + zoom * p`.

/// The most a picture is enlarged.
pub const MAX: f32 = 8.0;

/// How far one notch of a mouse wheel zooms: e^(120/300) ≈ 1.5×.
const WHEEL: f32 = 300.0;

/// Where the zoomed picture is.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct View {
    pub zoom: f32,
    pub x: f32,
    pub y: f32,
}

impl View {
    pub const WHOLE: View = View {
        zoom: 1.0,
        x: 0.0,
        y: 0.0,
    };

    pub fn zoomed(self) -> bool {
        self.zoom > 1.005
    }
}

/// The stage and the picture's own size in pixels.
#[derive(Clone, Copy, Debug)]
pub struct Frame {
    pub stage_w: f32,
    pub stage_h: f32,
    pub video_w: f32,
    pub video_h: f32,
}

/// The picture inside the unzoomed stage: left, top, width, height, and how
/// many stage pixels one picture pixel takes.
fn picture(f: Frame) -> (f32, f32, f32, f32, f32) {
    let (vw, vh) = if f.video_w > 0.0 && f.video_h > 0.0 {
        (f.video_w, f.video_h)
    } else {
        (f.stage_w, f.stage_h)
    };
    let k = if vw > 0.0 && vh > 0.0 {
        (f.stage_w / vw).min(f.stage_h / vh)
    } else {
        1.0
    };
    let (w, h) = (vw * k, vh * k);
    ((f.stage_w - w) / 2.0, (f.stage_h - h) / 2.0, w, h, k)
}

/// One axis: a picture larger than the stage may go as far as either of its
/// own edges, never past one into the bars; a smaller one stays centred.
/// `edge` keeps the picture's outermost row just off the stage, where the
/// scaler blends it into the bars (a hairline, seen on the web page).
fn axis(t: f32, zoom: f32, start: f32, size: f32, view: f32, edge: f32) -> f32 {
    let span = zoom * size;
    // Not wider than the stage by more than the two edges: centred. (Below
    // that the two bounds cross, and `clamp` would panic.)
    if span <= view + 2.0 * edge {
        return (view - span) / 2.0 - zoom * start;
    }
    t.clamp(view - zoom * (start + size) + edge, -zoom * start - edge)
}

/// `v` moved, if need be, so the picture stays inside its own edges.
pub fn keep_inside(v: View, f: Frame) -> View {
    if !v.zoomed() || f.stage_w <= 0.0 || f.stage_h <= 0.0 {
        return View::WHOLE;
    }
    let (px, py, pw, ph, k) = picture(f);
    let edge = v.zoom * (k + 1.0) / 2.0;
    View {
        zoom: v.zoom,
        x: axis(v.x, v.zoom, px, pw, f.stage_w, edge),
        y: axis(v.y, v.zoom, py, ph, f.stage_h, edge),
    }
}

/// Zoom to `level`, keeping the stage point (`at_x`, `at_y`) over the same bit
/// of picture.
pub fn around(v: View, level: f32, at_x: f32, at_y: f32, f: Frame) -> View {
    let z = level.clamp(1.0, MAX);
    let (qx, qy) = ((at_x - v.x) / v.zoom, (at_y - v.y) / v.zoom);
    keep_inside(
        View {
            zoom: z,
            x: at_x - z * qx,
            y: at_y - z * qy,
        },
        f,
    )
}

/// A wheel turn of `delta_y` (logical pixels, up is negative) at the pointer.
pub fn wheel(v: View, delta_y: f32, at_x: f32, at_y: f32, f: Frame) -> View {
    around(v, v.zoom * (-delta_y / WHEEL).exp(), at_x, at_y, f)
}

/// A drag of (`dx`, `dy`) from where the picture was at the press.
pub fn drag(from: View, dx: f32, dy: f32, f: Frame) -> View {
    keep_inside(
        View {
            zoom: from.zoom,
            x: from.x + dx,
            y: from.y + dy,
        },
        f,
    )
}

/// The chip's words: "2.0×".
pub fn label(v: View) -> String {
    if v.zoomed() {
        format!("{:.1}×", v.zoom)
    } else {
        "1×".to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::{View, around, drag, keep_inside, wheel};

    /// A 16:9 picture on a 1280×720 stage: it fills it exactly.
    const WIDE: super::Frame = super::Frame {
        stage_w: 1280.0,
        stage_h: 720.0,
        video_w: 640.0,
        video_h: 360.0,
    };
    /// A phone's upright picture on the same stage: bars left and right.
    const TALL: super::Frame = super::Frame {
        stage_w: 1280.0,
        stage_h: 720.0,
        video_w: 360.0,
        video_h: 640.0,
    };

    fn near(a: f32, b: f32) -> bool {
        (a - b).abs() < 0.01
    }

    #[test]
    fn zooming_keeps_the_point_under_the_pointer() {
        let v = around(View::WHOLE, 3.0, 500.0, 300.0, WIDE);
        assert!(near(v.zoom, 3.0));
        assert!(near(v.x + 3.0 * 500.0, 500.0), "{v:?}");
        assert!(near(v.y + 3.0 * 300.0, 300.0), "{v:?}");
    }

    #[test]
    fn a_drag_stops_at_the_pictures_own_edges() {
        let z = around(View::WHOLE, 4.0, 640.0, 360.0, WIDE);
        let far = drag(z, 1.0e5, 1.0e5, WIDE);
        // The picture's left and top edges sit at the stage's (just past it).
        assert!(far.x <= 0.0 && far.x > -8.0, "{far:?}");
        assert!(far.y <= 0.0 && far.y > -8.0, "{far:?}");
        let other = drag(z, -1.0e5, -1.0e5, WIDE);
        assert!(
            other.x + 4.0 * 1280.0 >= 1280.0 && other.x + 4.0 * 1280.0 < 1288.0,
            "{other:?}"
        );
    }

    #[test]
    fn a_narrow_picture_stays_centred_until_it_is_wider_than_the_stage() {
        // 360x640 fitted into 720 high is 405 wide, from x 437.5.
        let v = around(View::WHOLE, 2.0, 100.0, 360.0, TALL);
        // 810 wide < 1280: centred whatever the pointer.
        assert!(near(v.x + 2.0 * 437.5, (1280.0 - 810.0) / 2.0), "{v:?}");
        let moved = drag(v, 300.0, 0.0, TALL);
        assert!(near(moved.x, v.x), "no sideways room: {moved:?}");
    }

    #[test]
    fn a_picture_just_wider_than_the_stage_never_panics() {
        // The review's cases: an upright phone picture between 3.160x and
        // 3.177x, and a slight zoom of a picture that fills the stage.
        for z in [3.16_f32, 3.165, 3.17, 3.177] {
            let v = around(View::WHOLE, z, 640.0, 360.0, TALL);
            assert!(v.x.is_finite() && v.y.is_finite(), "{v:?}");
        }
        let f = super::Frame {
            stage_w: 1280.0,
            stage_h: 708.0,
            video_w: 640.0,
            video_h: 360.0,
        };
        for z in [1.0167_f32, 1.018, 1.0191] {
            let v = around(View::WHOLE, z, 100.0, 100.0, f);
            assert!(v.x.is_finite() && v.y.is_finite(), "{v:?}");
        }
    }

    #[test]
    fn the_wheel_zooms_in_and_back_out_to_whole() {
        let v = wheel(View::WHOLE, -120.0, 640.0, 360.0, WIDE);
        assert!(v.zoom > 1.4, "{v:?}");
        let back = wheel(v, 1200.0, 640.0, 360.0, WIDE);
        assert_eq!(back, View::WHOLE);
        assert_eq!(keep_inside(View::WHOLE, WIDE), View::WHOLE);
    }
}
