//! [`Grid`] — a 2-D occupancy-grid map, loadable from a small in-crate text
//! format, and consulted by [`crate::lidar`]'s raycast.
//!
//! # The text format
//!
//! ```text
//! ; Lines starting with `;`, and blank lines, are comments before the
//! ; header — never inside the grid body (see below for why).
//! 5 3 0.5
//! #####
//! #...#
//! #####
//! ```
//!
//! - The first non-comment, non-blank line is the **header**:
//!   `<width> <height> <resolution>`, whitespace-separated — `width`/
//!   `height` positive integers (cell counts), `resolution` a positive
//!   finite number (metres per cell edge).
//! - Exactly `height` lines follow immediately, each exactly `width`
//!   characters of `.` (free) or `#` (occupied) — no comment or blank line
//!   may appear *between* them, because a wall row is legitimately a line
//!   of `#` characters and this format would have no way to tell that
//!   apart from a comment line if comments were also introduced with `#`.
//!   Using `;` for comments (rather than the far more common `#`) is what
//!   sidesteps that ambiguity entirely, at the cost of looking slightly
//!   unfamiliar; restricting comments to *before* the header is what
//!   sidesteps needing to disambiguate a blank/comment line from a
//!   legitimate (if unusual) map row anywhere the grammar would otherwise
//!   have to guess.
//!
//! # Row order: the file's row order is the grid's row order
//!
//! The Nth map row in the text (0-indexed, counting from the line right
//! after the header) is grid row `y = N` — reading the file top-to-bottom
//! walks `y` from `0` upward, and nothing in this crate ever reverses that
//! correspondence. [`Grid`]'s internal storage is row-major (index `y *
//! width + x`, the same layout
//! [`astrs_node_api::message::OccupancyGrid::cells`] itself uses), so a
//! text row lands at its matching index with no transformation at all —
//! and [`Grid::to_occupancy_grid`] is consequently a straight copy, never
//! a row-reversal.
//!
//! A flip (making the *last* text row `y = 0` instead, so the file reads
//! like a picture with "up" at the top) was deliberately not chosen: it is
//! one more transformation for a format whose whole point is being simple
//! to write and to reason about, and one more place a row-order bug could
//! hide — which is exactly what happened during this crate's own
//! development, where an earlier draft's *docs* claimed the flipped
//! convention while its *code* already implemented this one, undetected
//! because the worked example used to check it was symmetric under both
//! readings. This module's own test suite deliberately uses an asymmetric
//! fixture for exactly that reason.
//!
//! Worked example (`width=3, height=2`):
//!
//! ```text
//! 3 2 1.0
//! ...
//! .#.
//! ```
//!
//! The first data row (`"..."`) is `y = 0`; the second (`".#."`) is `y =
//! 1`. So `cell(1, 0)` is `Some(false)` and `cell(1, 1)` is `Some(true)`.

use astrs_node_api::message::OccupancyGrid;

/// A [`Grid`] parsing or construction failure.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum GridError {
    /// The text held no header line at all (every line was blank, a
    /// comment, or the input was empty).
    #[error("the map text has no header line (width height resolution)")]
    MissingHeader,
    /// The header line did not have exactly three whitespace-separated
    /// fields.
    #[error("the map header `{line}` must have exactly 3 fields: width height resolution")]
    MalformedHeader {
        /// The offending header line, verbatim.
        line: String,
    },
    /// `width` or `height` did not parse as a positive integer.
    #[error("the map header `{line}`'s width/height must be positive integers")]
    InvalidDimensions {
        /// The offending header line, verbatim.
        line: String,
    },
    /// `resolution` did not parse as a positive, finite number.
    #[error("the map header's resolution must be a positive finite number, found `{value}`")]
    InvalidResolution {
        /// The raw (unparsed) resolution field.
        value: String,
    },
    /// Fewer map rows followed the header than `height` declared.
    #[error("expected {expected} map rows after the header, found only {found}")]
    RowCountMismatch {
        /// The declared height.
        expected: u32,
        /// How many rows were actually present.
        found: u32,
    },
    /// A map row's character count did not match the declared `width`.
    #[error("row {row} has {found} characters, expected exactly {expected} (the declared width)")]
    RowWidthMismatch {
        /// The row index (`0` = the first row after the header).
        row: u32,
        /// The declared width.
        expected: u32,
        /// The row's actual character count.
        found: usize,
    },
    /// A map row contained a character other than `.` or `#`.
    #[error(
        "row {row} column {col} has an unrecognized cell character `{ch}` (expected `.` or `#`)"
    )]
    UnknownCellChar {
        /// The row index.
        row: u32,
        /// The column index.
        col: u32,
        /// The offending character.
        ch: char,
    },
    /// [`Grid::new`] was called with a `cells` vector whose length does not
    /// match `width * height`.
    #[error("cells.len() == {found} does not match width * height == {expected}")]
    CellCountMismatch {
        /// The required cell count (`width * height`).
        expected: usize,
        /// The actual `cells.len()`.
        found: usize,
    },
}

/// A 2-D occupancy-grid map: a rectangular array of free/occupied cells at
/// a fixed metric resolution.
///
/// Purely a **kinematic** obstacle map, consulted only by [`crate::lidar`]'s
/// raycast: nothing in this crate stops a robot's commanded motion at a
/// wall (see [`crate::World`]'s own docs on this crate's kinematic-only
/// scope — there is no collision response here, only what the simulated
/// sensor sees).
#[derive(Debug, Clone, PartialEq)]
pub struct Grid {
    width: u32,
    height: u32,
    resolution: f64,
    origin: (f64, f64),
    /// Row-major, `len() == width * height`: `cells[y * width + x]`,
    /// `true` meaning occupied. See [module docs](self) for why row `0` is
    /// the row nearest `origin`, not the map's visual top.
    cells: Vec<bool>,
}

impl Grid {
    /// Builds a grid directly from its parts — the constructor
    /// [`Grid::from_text`] itself funnels into, and the one a caller
    /// assembling a map programmatically (rather than parsing one) uses.
    ///
    /// `origin` is the world-frame position of cell `(0, 0)`'s lower-left
    /// corner; [`Grid::from_text`] always uses `(0.0, 0.0)` (see that
    /// function's docs) — this constructor is where a caller who needs a
    /// map not anchored at the world origin sets it.
    ///
    /// # Errors
    ///
    /// [`GridError::InvalidDimensions`] if `width` or `height` is `0`;
    /// [`GridError::InvalidResolution`] if `resolution` is not positive and
    /// finite; [`GridError::CellCountMismatch`] if `cells.len() != width *
    /// height`.
    pub fn new(
        width: u32,
        height: u32,
        resolution: f64,
        origin: (f64, f64),
        cells: Vec<bool>,
    ) -> Result<Self, GridError> {
        if width == 0 || height == 0 {
            return Err(GridError::InvalidDimensions {
                line: format!("{width} {height} {resolution}"),
            });
        }
        if !resolution.is_finite() || resolution <= 0.0 {
            return Err(GridError::InvalidResolution {
                value: resolution.to_string(),
            });
        }
        // `u64` intermediate, then a saturating narrow: the same defensive
        // shape `astrs_node_api::message::OccupancyGrid::check_dimensions`
        // uses, so an adversarial `width * height` that would overflow
        // `usize` on a 32-bit target is reported as a mismatch rather than
        // silently wrapping.
        let expected = usize::try_from(u64::from(width) * u64::from(height)).unwrap_or(usize::MAX);
        if cells.len() != expected {
            return Err(GridError::CellCountMismatch {
                expected,
                found: cells.len(),
            });
        }
        Ok(Self {
            width,
            height,
            resolution,
            origin,
            cells,
        })
    }

    /// Parses [the text format](self) into a grid, anchored at world origin
    /// `(0.0, 0.0)`.
    ///
    /// # Errors
    ///
    /// See [`GridError`]'s variants.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_sim::grid::Grid;
    ///
    /// let grid = Grid::from_text("3 2 1.0\n...\n.#.\n").unwrap();
    /// assert_eq!((grid.width(), grid.height()), (3, 2));
    /// assert_eq!(grid.cell(1, 0), Some(false)); // row 0 = the *first* text line
    /// assert_eq!(grid.cell(1, 1), Some(true)); // row 1 = the second text line
    /// ```
    pub fn from_text(text: &str) -> Result<Self, GridError> {
        let mut lines = text.lines();
        let header_line = loop {
            match lines.next() {
                None => return Err(GridError::MissingHeader),
                Some(raw) => {
                    let trimmed = raw.trim();
                    if trimmed.is_empty() || trimmed.starts_with(';') {
                        continue;
                    }
                    break trimmed;
                }
            }
        };

        let fields: Vec<&str> = header_line.split_whitespace().collect();
        if fields.len() != 3 {
            return Err(GridError::MalformedHeader {
                line: header_line.to_owned(),
            });
        }
        let width: u32 = fields[0].parse().ok().filter(|w| *w > 0).ok_or_else(|| {
            GridError::InvalidDimensions {
                line: header_line.to_owned(),
            }
        })?;
        let height: u32 = fields[1].parse().ok().filter(|h| *h > 0).ok_or_else(|| {
            GridError::InvalidDimensions {
                line: header_line.to_owned(),
            }
        })?;
        let resolution: f64 = fields[2]
            .parse()
            .ok()
            .filter(|r: &f64| r.is_finite() && *r > 0.0)
            .ok_or_else(|| GridError::InvalidResolution {
                value: fields[2].to_owned(),
            })?;

        let mut cells = Vec::with_capacity(width as usize * height as usize);
        for row in 0..height {
            // `row` (0-indexed) already equals how many rows were
            // successfully read before this iteration, so it doubles as
            // the "found" count on the early-exit path below — no
            // separate counter needed.
            let Some(raw_line) = lines.next() else {
                return Err(GridError::RowCountMismatch {
                    expected: height,
                    found: row,
                });
            };
            let line = raw_line.strip_suffix('\r').unwrap_or(raw_line);
            let row_chars: Vec<char> = line.chars().collect();
            if row_chars.len() != width as usize {
                return Err(GridError::RowWidthMismatch {
                    row,
                    expected: width,
                    found: row_chars.len(),
                });
            }
            for (col, ch) in row_chars.into_iter().enumerate() {
                let occupied = match ch {
                    '.' => false,
                    '#' => true,
                    other => {
                        return Err(GridError::UnknownCellChar {
                            row,
                            col: u32::try_from(col).unwrap_or(u32::MAX),
                            ch: other,
                        });
                    }
                };
                cells.push(occupied);
            }
        }

        Self::new(width, height, resolution, (0.0, 0.0), cells)
    }

    /// The grid's width, in cells.
    #[must_use]
    pub const fn width(&self) -> u32 {
        self.width
    }

    /// The grid's height, in cells.
    #[must_use]
    pub const fn height(&self) -> u32 {
        self.height
    }

    /// The size of one cell's edge, in metres.
    #[must_use]
    pub const fn resolution(&self) -> f64 {
        self.resolution
    }

    /// The world-frame position of cell `(0, 0)`'s lower-left corner.
    #[must_use]
    pub const fn origin(&self) -> (f64, f64) {
        self.origin
    }

    /// Whether `(gx, gy)` names a cell this grid actually holds.
    #[must_use]
    pub fn in_bounds(&self, gx: i64, gy: i64) -> bool {
        gx >= 0
            && gy >= 0
            && (gx as u64) < u64::from(self.width)
            && (gy as u64) < u64::from(self.height)
    }

    /// The occupancy of cell `(gx, gy)` — `None` when it names a cell
    /// outside the grid rather than treating out-of-bounds as either
    /// occupied or free (see [`crate::lidar`]'s raycast for why that
    /// distinction matters: a ray that leaves the mapped area reports "no
    /// return", not "hit a wall at the boundary").
    #[must_use]
    pub fn cell(&self, gx: i64, gy: i64) -> Option<bool> {
        if !self.in_bounds(gx, gy) {
            return None;
        }
        // `in_bounds` already guarantees both are non-negative and within
        // `u32` range, so these casts are exact.
        let index = (gy as usize) * (self.width as usize) + (gx as usize);
        self.cells.get(index).copied()
    }

    /// Converts a world-frame point to continuous grid coordinates (cell
    /// units, not necessarily integral — the point may sit anywhere inside
    /// a cell, or outside the grid altogether).
    #[must_use]
    pub fn world_to_grid(&self, x: f64, y: f64) -> (f64, f64) {
        (
            (x - self.origin.0) / self.resolution,
            (y - self.origin.1) / self.resolution,
        )
    }

    /// The world-frame position of cell `(gx, gy)`'s center — the inverse
    /// of [`Grid::world_to_grid`] at a cell's midpoint.
    #[must_use]
    pub fn grid_cell_center(&self, gx: u32, gy: u32) -> (f64, f64) {
        (
            self.origin.0 + (f64::from(gx) + 0.5) * self.resolution,
            self.origin.1 + (f64::from(gy) + 0.5) * self.resolution,
        )
    }

    /// Converts this grid to the wire [`OccupancyGrid`] type: `true`
    /// (occupied) becomes `100`, `false` (free) becomes `0` — this crate's
    /// [`Grid`] models no "unknown" cell, so `OccupancyGrid`'s third
    /// state (`-1`) never appears.
    ///
    /// A library-level conversion, not one this crate wires to a node
    /// output port — see [`crate::World`]'s own docs on why the sim node
    /// publishes `scan`/`odom`/`tf` and nothing map-shaped: a caller
    /// building a viewer, a test fixture, or a richer tutorial node can
    /// still reach the wire shape directly.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_sim::grid::Grid;
    ///
    /// let grid = Grid::from_text("2 1 1.0\n.#\n").unwrap();
    /// let wire = grid.to_occupancy_grid();
    /// assert_eq!((wire.width, wire.height), (2, 1));
    /// assert_eq!(wire.cells, vec![0, 100]);
    /// ```
    #[must_use]
    pub fn to_occupancy_grid(&self) -> OccupancyGrid {
        use astrs_node_api::message::{Pose, Quaternion, Vector3};
        let cells: Vec<i8> = self
            .cells
            .iter()
            .map(|&occupied| if occupied { 100 } else { 0 })
            .collect();
        let origin = Pose::new(
            Vector3::new(self.origin.0, self.origin.1, 0.0),
            Quaternion::IDENTITY,
        );
        // `width`/`height`/`cells.len()` were already checked consistent
        // at construction (`Grid::new`'s own `CellCountMismatch` guard), so
        // `OccupancyGrid::new`'s own dimension check cannot fail here — but
        // this workspace denies `unwrap`/`expect`, so the "impossible"
        // branch below still has to be a real (if unreachable in practice)
        // value rather than an assertion.
        //
        // Narrowing to `f32` (`std/nav/v1/OccupancyGrid`'s own wire
        // contract for `resolution`) is an accepted precision loss for a
        // metres-per-cell value, not a truncation bug.
        let resolution = self.resolution as f32;
        OccupancyGrid::new(resolution, self.width, self.height, origin, cells).unwrap_or_else(
            |_| OccupancyGrid {
                resolution,
                width: self.width,
                height: self.height,
                origin: Pose::default(),
                cells: vec![-1; self.cells.len()],
            },
        )
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    const SAMPLE: &str = "5 3 0.5\n#####\n#...#\n#####\n";

    #[test]
    fn a_well_formed_map_parses() {
        let grid = Grid::from_text(SAMPLE).unwrap();
        assert_eq!(grid.width(), 5);
        assert_eq!(grid.height(), 3);
        assert_eq!(grid.resolution(), 0.5);
        assert_eq!(grid.origin(), (0.0, 0.0));
    }

    /// The worked example from the module docs, made executable. Uses its
    /// own fixture rather than [`SAMPLE`] on purpose: `SAMPLE`'s first and
    /// last rows are identical ("#####" both times), so a test built on it
    /// alone would pass under *either* row-order convention and prove
    /// nothing — exactly the gap that let this crate's own docs and code
    /// disagree undetected during development (see [module docs](self)).
    /// This fixture's two rows differ, so the test fails outright if a
    /// future edit ever reverses the convention.
    #[test]
    fn the_first_text_row_after_the_header_is_grid_row_y_equals_zero() {
        let grid = Grid::from_text("3 2 1.0\n###\n...\n").unwrap();
        // The FIRST text row ("###") is grid row y=0.
        assert_eq!(grid.cell(0, 0), Some(true));
        assert_eq!(grid.cell(1, 0), Some(true));
        assert_eq!(grid.cell(2, 0), Some(true));
        // The SECOND text row ("...") is grid row y=1.
        assert_eq!(grid.cell(0, 1), Some(false));
        assert_eq!(grid.cell(1, 1), Some(false));
        assert_eq!(grid.cell(2, 1), Some(false));
    }

    #[test]
    fn out_of_bounds_cells_are_none_not_free_or_occupied() {
        let grid = Grid::from_text(SAMPLE).unwrap();
        assert_eq!(grid.cell(-1, 0), None);
        assert_eq!(grid.cell(0, -1), None);
        assert_eq!(grid.cell(5, 0), None);
        assert_eq!(grid.cell(0, 3), None);
        assert!(!grid.in_bounds(5, 0));
        assert!(grid.in_bounds(4, 2));
    }

    #[test]
    fn comments_and_blank_lines_before_the_header_are_skipped() {
        let text = "; a comment\n\n  \n; another\n3 1 1.0\n.#.\n";
        let grid = Grid::from_text(text).unwrap();
        assert_eq!((grid.width(), grid.height()), (3, 1));
        assert_eq!(grid.cell(1, 0), Some(true));
    }

    #[test]
    fn a_carriage_return_line_ending_is_tolerated() {
        let text = "2 1 1.0\r\n.#\r\n";
        let grid = Grid::from_text(text).unwrap();
        assert_eq!(grid.cell(1, 0), Some(true));
    }

    #[test]
    fn an_empty_input_reports_a_missing_header() {
        assert_eq!(Grid::from_text(""), Err(GridError::MissingHeader));
        assert_eq!(
            Grid::from_text("; only comments\n"),
            Err(GridError::MissingHeader)
        );
    }

    #[test]
    fn a_header_with_the_wrong_field_count_is_rejected() {
        let error = Grid::from_text("5 3\n").unwrap_err();
        assert_eq!(
            error,
            GridError::MalformedHeader {
                line: "5 3".to_owned()
            }
        );
    }

    #[test]
    fn non_positive_or_unparseable_dimensions_are_rejected() {
        for header in ["0 3 1.0", "5 0 1.0", "x 3 1.0", "5 x 1.0", "-1 3 1.0"] {
            let text = format!("{header}\n");
            assert!(
                matches!(
                    Grid::from_text(&text),
                    Err(GridError::InvalidDimensions { .. })
                ),
                "{header}"
            );
        }
    }

    #[test]
    fn a_non_positive_or_non_finite_resolution_is_rejected() {
        for value in ["0.0", "-1.0", "nan", "inf"] {
            let text = format!("2 2 {value}\n##\n##\n");
            assert!(
                matches!(
                    Grid::from_text(&text),
                    Err(GridError::InvalidResolution { .. })
                ),
                "{value}"
            );
        }
    }

    #[test]
    fn too_few_map_rows_is_a_row_count_mismatch() {
        let text = "2 2 1.0\n##\n";
        assert_eq!(
            Grid::from_text(text),
            Err(GridError::RowCountMismatch {
                expected: 2,
                found: 1
            })
        );
    }

    #[test]
    fn a_row_of_the_wrong_width_is_rejected() {
        let text = "3 1 1.0\n##\n";
        assert_eq!(
            Grid::from_text(text),
            Err(GridError::RowWidthMismatch {
                row: 0,
                expected: 3,
                found: 2
            })
        );
    }

    #[test]
    fn an_unrecognized_cell_character_is_rejected() {
        let text = "3 1 1.0\n.X.\n";
        assert_eq!(
            Grid::from_text(text),
            Err(GridError::UnknownCellChar {
                row: 0,
                col: 1,
                ch: 'X'
            })
        );
    }

    #[test]
    fn extra_trailing_content_after_the_grid_body_is_ignored() {
        let text = "2 1 1.0\n.#\n; trailing comment\nrandom trailer\n";
        let grid = Grid::from_text(text).unwrap();
        assert_eq!(grid.cell(1, 0), Some(true));
    }

    #[test]
    fn world_to_grid_and_grid_cell_center_are_consistent_with_resolution() {
        let grid = Grid::new(4, 4, 0.5, (1.0, 2.0), vec![false; 16]).unwrap();
        assert_eq!(grid.world_to_grid(1.0, 2.0), (0.0, 0.0));
        assert_eq!(grid.world_to_grid(3.0, 4.0), (4.0, 4.0));
        let center = grid.grid_cell_center(0, 0);
        assert_eq!(center, (1.25, 2.25));
        // The center converts back to grid coordinates at exactly (0.5, 0.5)
        // — the middle of cell (0, 0).
        assert_eq!(grid.world_to_grid(center.0, center.1), (0.5, 0.5));
    }

    #[test]
    fn grid_new_rejects_a_mismatched_cell_count() {
        let error = Grid::new(2, 2, 1.0, (0.0, 0.0), vec![false; 3]).unwrap_err();
        assert_eq!(
            error,
            GridError::CellCountMismatch {
                expected: 4,
                found: 3
            }
        );
    }

    #[test]
    fn grid_new_rejects_zero_dimensions_and_bad_resolution() {
        assert!(matches!(
            Grid::new(0, 3, 1.0, (0.0, 0.0), vec![]),
            Err(GridError::InvalidDimensions { .. })
        ));
        assert!(matches!(
            Grid::new(3, 3, 0.0, (0.0, 0.0), vec![false; 9]),
            Err(GridError::InvalidResolution { .. })
        ));
        assert!(matches!(
            Grid::new(3, 3, f64::NAN, (0.0, 0.0), vec![false; 9]),
            Err(GridError::InvalidResolution { .. })
        ));
    }

    #[test]
    fn to_occupancy_grid_maps_free_and_occupied_to_zero_and_a_hundred() {
        let grid = Grid::from_text(SAMPLE).unwrap();
        let wire = grid.to_occupancy_grid();
        assert_eq!(wire.width, 5);
        assert_eq!(wire.height, 3);
        assert_eq!(wire.resolution, 0.5);
        assert!(wire.cells.iter().all(|&c| c == 0 || c == 100));
        assert_eq!(wire.cells.len(), 15);
        // No "unknown" (-1) cell is ever produced.
        assert!(!wire.cells.contains(&-1));
    }
}
