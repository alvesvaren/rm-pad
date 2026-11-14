//! Stackable pen-coordinate mapping.
//!
//! Each [`PenInputMap`] is one stage in a fixed-order pipeline that transforms
//! orientation-corrected pen coordinates on their way to the uinput device. A
//! stage maps a point from its input coordinate space to its output space and
//! reports how its output axis bounds derive from its input bounds.
//!
//! [`PenInputPipeline`] chains stages: it seeds with the pen's oriented
//! dimensions, folds each stage's [`PenInputMap::output_bounds`] to compute the
//! final uinput axis range, and folds [`PenInputMap::map`] per sample. An empty
//! pipeline is the identity over the seed bounds, which reproduces the plain
//! whole-desktop stretch used when no mapping applies.

/// One stage of pen-coordinate transformation.
pub trait PenInputMap {
    /// Map a point from this stage's input space (`0..=in_x_max`,
    /// `0..=in_y_max`) into its output space.
    fn map(&self, x: i32, y: i32, in_x_max: i32, in_y_max: i32) -> (i32, i32);

    /// The output axis bounds this stage produces from the given input bounds.
    /// Stages that define their own output space (e.g. a display) ignore the
    /// input; pass-through stages return the input unchanged.
    fn output_bounds(&self, in_x_max: i32, in_y_max: i32) -> (i32, i32);

    /// Short human-readable description, for startup logging.
    fn label(&self) -> String;
}

/// A stage together with the input bounds it receives within the pipeline.
struct Stage {
    map: Box<dyn PenInputMap>,
    in_x_max: i32,
    in_y_max: i32,
}

/// A fixed-order chain of [`PenInputMap`] stages.
pub struct PenInputPipeline {
    stages: Vec<Stage>,
    /// Final output axis maximums, i.e. the uinput ABS_X/ABS_Y ranges.
    pub axis_x_max: i32,
    pub axis_y_max: i32,
}

impl PenInputPipeline {
    /// Build a pipeline seeded with the pen's oriented dimensions. Stage input
    /// bounds and the final axis range are derived by folding `output_bounds`.
    pub fn new(seed_x_max: i32, seed_y_max: i32, maps: Vec<Box<dyn PenInputMap>>) -> Self {
        let mut in_x = seed_x_max;
        let mut in_y = seed_y_max;
        let mut stages = Vec::with_capacity(maps.len());

        for map in maps {
            let (out_x, out_y) = map.output_bounds(in_x, in_y);
            stages.push(Stage { map, in_x_max: in_x, in_y_max: in_y });
            in_x = out_x;
            in_y = out_y;
        }

        Self { stages, axis_x_max: in_x, axis_y_max: in_y }
    }

    /// Fold a pen coordinate through every stage in order.
    pub fn map(&self, x: i32, y: i32) -> (i32, i32) {
        let mut point = (x, y);
        for stage in &self.stages {
            point = stage.map.map(point.0, point.1, stage.in_x_max, stage.in_y_max);
        }
        point
    }

    /// Describe the stage chain for logging, e.g. `identity` or `HDMI-A-1 ...`.
    pub fn describe(&self) -> String {
        if self.stages.is_empty() {
            return "identity (whole desktop)".to_string();
        }
        self.stages
            .iter()
            .map(|s| s.map.label())
            .collect::<Vec<_>>()
            .join(" -> ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Offsets a point by a fixed amount and rescales its bounds by a factor,
    /// enough to observe ordering and bounds folding.
    struct Shift {
        dx: i32,
        dy: i32,
        out_x: i32,
        out_y: i32,
    }

    impl PenInputMap for Shift {
        fn map(&self, x: i32, y: i32, _in_x_max: i32, _in_y_max: i32) -> (i32, i32) {
            (x + self.dx, y + self.dy)
        }
        fn output_bounds(&self, _in_x_max: i32, _in_y_max: i32) -> (i32, i32) {
            (self.out_x, self.out_y)
        }
        fn label(&self) -> String {
            format!("shift({},{})", self.dx, self.dy)
        }
    }

    #[test]
    fn empty_pipeline_is_identity_over_seed_bounds() {
        let p = PenInputPipeline::new(100, 200, vec![]);
        assert_eq!((p.axis_x_max, p.axis_y_max), (100, 200));
        assert_eq!(p.map(37, 42), (37, 42));
        assert_eq!(p.describe(), "identity (whole desktop)");
    }

    #[test]
    fn folds_bounds_and_applies_stages_in_order() {
        let maps: Vec<Box<dyn PenInputMap>> = vec![
            Box::new(Shift { dx: 1, dy: 2, out_x: 1000, out_y: 2000 }),
            Box::new(Shift { dx: 10, dy: 20, out_x: 5, out_y: 6 }),
        ];
        let p = PenInputPipeline::new(100, 200, maps);
        // Final axis bounds come from the last stage's output_bounds.
        assert_eq!((p.axis_x_max, p.axis_y_max), (5, 6));
        // Both shifts apply, in order: +1/+2 then +10/+20.
        assert_eq!(p.map(0, 0), (11, 22));
        assert_eq!(p.describe(), "shift(1,2) -> shift(10,20)");
    }
}
