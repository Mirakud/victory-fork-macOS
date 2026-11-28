//! Conservative rejection of complete receiver volumes outside the game camera.
//! Points are relative to the camera before projection, including projection jitter.
pub(super) struct ReceiverFrustum {
    origin: [f64; 3],
    // A world differential projected onto one homogeneous clip half-space.
    planes: [[f64; 4]; 5],
}

impl ReceiverFrustum {
    pub(super) fn new(camera: &[f32; 20], projection: &[f32; 16]) -> Option<Self> {
        if !projection
            .iter()
            .chain(camera.iter())
            .all(|v| v.is_finite())
        {
            return None;
        }
        let origin = std::array::from_fn(|i| f64::from(camera[16 + i]));
        // w, w+x, w-x, w+y, w-y. The distant plane is intentionally unbounded.
        let clip = [(0, 0.0), (0, 1.0), (0, -1.0), (1, 1.0), (1, -1.0)];
        let planes = clip.map(|(axis, sign)| {
            let q: [f64; 4] = std::array::from_fn(|r| {
                f64::from(projection[4 * r + 3]) + sign * f64::from(projection[4 * r + axis])
            });
            let mut plane = [0.0; 4];
            for i in 0..3 {
                plane[i] = (0..3).map(|j| f64::from(camera[4 * i + j]) * q[j]).sum();
            }
            plane[3] = q[3];
            plane
        });
        Some(Self { origin, planes })
    }

    pub(super) fn sees(&self, min: &[f32; 4], max: &[f32; 4]) -> bool {
        self.planes.iter().all(|plane| {
            let mut upper = plane[3];
            for i in 0..3 {
                let corner = if plane[i] >= 0.0 { max[i] } else { min[i] };
                upper += plane[i] * (f64::from(corner) - self.origin[i]);
            }
            // Expand by one centimetre per axis to retain touching surfaces
            // despite the f32 G-buffer reconstruction at absolute world coordinates.
            upper >= -0.01 * (plane[0].abs() + plane[1].abs() + plane[2].abs())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view() -> ReceiverFrustum {
        let camera = [
            1., 0., 0., 0., 0., 1., 0., 0., 0., 0., 1., 0., 1., 1., 0., 0.05, 0., 0., 0., 0.,
        ];
        let projection = [
            1., 0., 0., 0., 0., 1., 0., 0., 0., 0., 0., -1., 0., 0., 0.05, 0.,
        ];
        ReceiverFrustum::new(&camera, &projection).unwrap()
    }

    #[test]
    fn covers_near_camera_containment_and_frustum_crossing() {
        let f = view();
        assert!(f.sees(&[-10., -10., -2., 0.], &[10., 10., -1., 0.])); // no corner inside
        assert!(f.sees(&[-1., -1., -1., 0.], &[1., 1., 1., 0.]));
        assert!(f.sees(&[2., -1., -2., 0.], &[3., 1., -1., 0.])); // touches right frustum
        assert!(f.sees(&[-1., -1., -10000., 0.], &[1., 1., -9999., 0.]));
    }

    #[test]
    fn rejects_boxes_beyond_each_of_the_five_planes() {
        let f = view();
        for (lo, hi) in [
            ([2.5, -0.2, -2., 0.], [3., 0.2, -1., 0.]),
            ([-3., -0.2, -2., 0.], [-2.5, 0.2, -1., 0.]),
            ([-0.2, 2.5, -2., 0.], [0.2, 3., -1., 0.]),
            ([-0.2, -3., -2., 0.], [0.2, -2.5, -1., 0.]),
            ([-1., -1., 2., 0.], [1., 1., 3., 0.]),
        ] {
            assert!(!f.sees(&lo, &hi));
        }
    }
}
