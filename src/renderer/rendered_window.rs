use std::sync::Arc;

use skia_safe::{
    canvas::SaveLayerRec, gpu::SurfaceOrigin, image_filters::blur, BlendMode, Budgeted, Canvas,
    Color, ImageInfo, Matrix, Paint, Picture, PictureRecorder, Point, Rect, Surface, SurfaceProps,
    SurfacePropsFlags,
};

use crate::{
    dimensions::Dimensions,
    editor::Style,
    profiling::tracy_zone,
    redraw_scheduler::REDRAW_SCHEDULER,
    renderer::{animation_utils::*, GridRenderer, RendererSettings},
};
use winit::dpi::PhysicalSize;

use super::opengl::clamp_render_buffer_size;

#[derive(Clone, Debug)]
pub struct LineFragment {
    pub text: String,
    pub window_left: u64,
    pub width: u64,
    pub style: Option<Arc<Style>>,
}

#[derive(Clone, Debug)]
pub enum WindowDrawCommand {
    Position {
        grid_position: (f64, f64),
        grid_size: (u64, u64),
        floating_order: Option<u64>,
    },
    DrawLine {
        row: usize,
        line_fragments: Vec<LineFragment>,
    },
    Scroll {
        top: u64,
        bottom: u64,
        left: u64,
        right: u64,
        rows: i64,
        cols: i64,
    },
    Clear,
    Show,
    Hide,
    Close,
    Viewport {
        scroll_delta: f64,
    },
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct WindowPadding {
    pub top: u32,
    pub left: u32,
    pub right: u32,
    pub bottom: u32,
}

fn build_window_surface(parent_canvas: &mut Canvas, pixel_size: PhysicalSize<u32>) -> Surface {
    let pixel_size = clamp_render_buffer_size(pixel_size);
    let mut context = parent_canvas.recording_context().unwrap();
    let budgeted = Budgeted::Yes;
    let parent_image_info = parent_canvas.image_info();
    let image_info = ImageInfo::new(
        (pixel_size.width as i32, pixel_size.height as i32),
        parent_image_info.color_type(),
        parent_image_info.alpha_type(),
        parent_image_info.color_space(),
    );
    let surface_origin = SurfaceOrigin::TopLeft;
    // Subpixel layout (should be configurable/obtained from fontconfig).
    let props = SurfaceProps::new(SurfacePropsFlags::default(), skia_safe::PixelGeometry::RGBH);
    Surface::new_render_target(
        &mut context,
        budgeted,
        &image_info,
        None,
        surface_origin,
        Some(&props),
        None,
    )
    .expect("Could not create surface")
}

fn build_window_surface_with_grid_size(
    parent_canvas: &mut Canvas,
    grid_renderer: &GridRenderer,
    grid_size: Dimensions,
) -> Surface {
    let mut surface = build_window_surface(
        parent_canvas,
        (grid_size * grid_renderer.font_dimensions).into(),
    );

    let canvas = surface.canvas();
    canvas.clear(grid_renderer.get_default_background());
    surface
}

pub struct LocatedSurface {
    surface: Surface,
    pub vertical_position: f32,
}

impl LocatedSurface {
    fn new(
        parent_canvas: &mut Canvas,
        grid_renderer: &GridRenderer,
        grid_size: Dimensions,
        vertical_position: f32,
    ) -> LocatedSurface {
        let surface = build_window_surface_with_grid_size(parent_canvas, grid_renderer, grid_size);

        LocatedSurface {
            surface,
            vertical_position,
        }
    }
}

#[derive(Clone)]
struct Line {
    background_picture: Option<Picture>,
    foreground_picture: Picture,
    has_transparency: bool,
}

pub struct RenderedWindow {
    pub current_surface: LocatedSurface,

    pub id: u64,
    pub hidden: bool,
    pub floating_order: Option<u64>,

    pub grid_size: Dimensions,

    lines: Vec<Option<Line>>,
    pub top_index: isize,

    grid_start_position: Point,
    pub grid_current_position: Point,
    grid_destination: Point,
    position_t: f32,

    pub scroll_animation: CriticallyDampedSpringAnimation,

    pub padding: WindowPadding,
}

#[derive(Clone, Debug)]
pub struct WindowDrawDetails {
    pub id: u64,
    pub region: Rect,
    pub floating_order: Option<u64>,
}

impl RenderedWindow {
    pub fn new(
        parent_canvas: &mut Canvas,
        grid_renderer: &GridRenderer,
        id: u64,
        grid_position: Point,
        grid_size: Dimensions,
        padding: WindowPadding,
    ) -> RenderedWindow {
        let current_surface = LocatedSurface::new(parent_canvas, grid_renderer, grid_size, 0.);

        RenderedWindow {
            current_surface,
            id,
            hidden: false,
            floating_order: None,

            grid_size,

            lines: vec![None; (grid_size.height * 2) as usize],
            top_index: 0,

            grid_start_position: grid_position,
            grid_current_position: grid_position,
            grid_destination: grid_position,
            position_t: 2.0, // 2.0 is out of the 0.0 to 1.0 range and stops animation.

            scroll_animation: CriticallyDampedSpringAnimation::new(),

            padding,
        }
    }

    pub fn pixel_region(&self, font_dimensions: Dimensions) -> Rect {
        let current_pixel_position = Point::new(
            self.grid_current_position.x * font_dimensions.width as f32,
            self.grid_current_position.y * font_dimensions.height as f32,
        );

        let image_size: (i32, i32) = (self.grid_size * font_dimensions).into();

        Rect::from_point_and_size(current_pixel_position, image_size)
    }

    pub fn update(&mut self, settings: &RendererSettings, dt: f32) -> bool {
        let mut animating = false;

        if 1.0 - self.position_t < std::f32::EPSILON {
            // We are at destination, move t out of 0-1 range to stop the animation.
            self.position_t = 2.0;
        } else {
            animating = true;
            self.position_t = (self.position_t + dt / settings.position_animation_length).min(1.0);
        }

        self.grid_current_position = ease_point(
            ease_out_expo,
            self.grid_start_position,
            self.grid_destination,
            self.position_t,
        );

        let timestep = 0.01;
        let mut dt = dt;
        let mut scrolling = false;
        while dt > 0.0 {
            scrolling = self
                .scroll_animation
                .update(dt, settings.scroll_animation_length);
            dt -= timestep;
        }

        animating | scrolling
    }

    fn draw_surface(&mut self, font_dimensions: Dimensions, default_background: Color) -> bool {
        let image_size: (i32, i32) = (self.grid_size * font_dimensions).into();
        let pixel_region = Rect::from_size(image_size);
        let canvas = self.current_surface.surface.canvas();
        canvas.clip_rect(pixel_region, None, Some(false));
        canvas.clear(default_background);

        let scroll_offset_lines = self.scroll_animation.position.floor();
        let scroll_offset = scroll_offset_lines - self.scroll_animation.position;
        let mut has_transparency = false;

        let mut background_paint = Paint::default();
        background_paint.set_blend_mode(BlendMode::Src);
        background_paint.set_alpha(default_background.a());

        let lines: Vec<(Matrix, &Line)> = (0..self.grid_size.height + 1)
            .filter_map(|i| {
                let line_index = (self.top_index + scroll_offset_lines as isize + i as isize)
                    .rem_euclid(self.lines.len() as isize)
                    as usize;
                if let Some(line) = &self.lines[line_index] {
                    let mut m = Matrix::new_identity();
                    m.set_translate((
                        0.0,
                        (scroll_offset + i as f32) * font_dimensions.height as f32,
                    ));
                    Some((m, line))
                } else {
                    None
                }
            })
            .collect();

        for (matrix, line) in &lines {
            if let Some(background_picture) = &line.background_picture {
                has_transparency |= line.has_transparency;
                canvas.draw_picture(background_picture, Some(matrix), Some(&background_paint));
            }
        }
        let mut foreground_paint = Paint::default();
        foreground_paint.set_blend_mode(BlendMode::SrcOver);
        for (matrix, line) in &lines {
            canvas.draw_picture(
                &line.foreground_picture,
                Some(matrix),
                Some(&foreground_paint),
            );
        }
        has_transparency
    }

    pub fn draw(
        &mut self,
        root_canvas: &mut Canvas,
        settings: &RendererSettings,
        default_background: Color,
        font_dimensions: Dimensions,
        dt: f32,
    ) -> WindowDrawDetails {
        if self.update(settings, dt) {
            REDRAW_SCHEDULER.queue_next_frame();
        }

        let has_transparency = self.draw_surface(font_dimensions, default_background);

        let pixel_region = self.pixel_region(font_dimensions);
        let transparent_floating = self.floating_order.is_some() && has_transparency;

        root_canvas.save();
        root_canvas.clip_rect(pixel_region, None, Some(false));
        let need_blur = transparent_floating && settings.floating_blur;

        if need_blur {
            if let Some(blur) = blur(
                (
                    settings.floating_blur_amount_x,
                    settings.floating_blur_amount_y,
                ),
                None,
                None,
                None,
            ) {
                let paint = Paint::default()
                    .set_anti_alias(false)
                    .set_blend_mode(BlendMode::Src)
                    .to_owned();
                let save_layer_rec = SaveLayerRec::default()
                    .backdrop(&blur)
                    .bounds(&pixel_region)
                    .paint(&paint);
                root_canvas.save_layer(&save_layer_rec);
                root_canvas.restore();
            }
        }

        let paint = Paint::default()
            .set_anti_alias(false)
            .set_color(Color::from_argb(255, 255, 255, 255))
            .set_blend_mode(if self.floating_order.is_some() {
                BlendMode::SrcOver
            } else {
                BlendMode::Src
            })
            .to_owned();

        // Draw current surface.
        let snapshot = self.current_surface.surface.image_snapshot();
        root_canvas.draw_image_rect(snapshot, None, pixel_region, &paint);

        root_canvas.restore();

        WindowDrawDetails {
            id: self.id,
            region: pixel_region,
            floating_order: self.floating_order,
        }
    }

    pub fn handle_window_draw_command(
        &mut self,
        grid_renderer: &mut GridRenderer,
        draw_command: WindowDrawCommand,
    ) {
        match draw_command {
            WindowDrawCommand::Position {
                grid_position: (grid_left, grid_top),
                grid_size,
                floating_order,
            } => {
                tracy_zone!("position_cmd", 0);
                let Dimensions {
                    width: font_width,
                    height: font_height,
                } = grid_renderer.font_dimensions;

                let top_offset = self.padding.top as f32 / font_height as f32;
                let left_offset = self.padding.left as f32 / font_width as f32;

                let grid_left = grid_left.max(0.0);
                let grid_top = grid_top.max(0.0);
                let new_destination: Point =
                    (grid_left as f32 + left_offset, grid_top as f32 + top_offset).into();
                let new_grid_size: Dimensions = grid_size.into();

                if self.grid_destination != new_destination {
                    if self.grid_start_position.x.abs() > f32::EPSILON
                        || self.grid_start_position.y.abs() > f32::EPSILON
                    {
                        self.position_t = 0.0; // Reset animation as we have a new destination.
                        self.grid_start_position = self.grid_current_position;
                    } else {
                        // We don't want to animate since the window is animating out of the start location,
                        // so we set t to 2.0 to stop animations.
                        self.position_t = 2.0;
                        self.grid_start_position = new_destination;
                    }
                    self.grid_destination = new_destination;
                }

                if self.grid_size != new_grid_size {
                    self.current_surface.surface = build_window_surface_with_grid_size(
                        self.current_surface.surface.canvas(),
                        grid_renderer,
                        new_grid_size,
                    );
                    self.grid_size = new_grid_size;
                }

                // This could perhaps be optimized, setting the position does not necessarily need
                // to resize and reset everything. See editor::window::Window::position for the
                // corresponding code on the logic side.
                self.lines = vec![None; (new_grid_size.height * 2) as usize];
                self.top_index = 0;

                self.floating_order = floating_order;

                if self.hidden {
                    self.hidden = false;
                    self.position_t = 2.0; // We don't want to animate since the window is becoming visible,
                                           // so we set t to 2.0 to stop animations.
                    self.grid_start_position = new_destination;
                    self.grid_destination = new_destination;
                }
                self.scroll_animation.reset();
            }
            WindowDrawCommand::DrawLine {
                row,
                line_fragments,
            } => {
                tracy_zone!("draw_line_cmd", 0);
                let font_dimensions = grid_renderer.font_dimensions;
                let mut recorder = PictureRecorder::new();

                let grid_rect = Rect::from_wh(
                    (self.grid_size.width * font_dimensions.width) as f32,
                    font_dimensions.height as f32,
                );
                let canvas = recorder.begin_recording(grid_rect, None);

                let line_index =
                    (self.top_index + row as isize).rem_euclid(self.lines.len() as isize) as usize;

                canvas.clear(grid_renderer.get_default_background());
                let mut has_transparency = false;
                let mut custom_background = false;

                for line_fragment in line_fragments.iter() {
                    let LineFragment {
                        window_left,
                        width,
                        style,
                        ..
                    } = line_fragment;
                    let grid_position = (*window_left, 0);
                    let (custom, transparent) =
                        grid_renderer.draw_background(canvas, grid_position, *width, style);
                    custom_background |= custom;
                    has_transparency |= transparent;
                }
                let background_picture = custom_background
                    .then_some(recorder.finish_recording_as_picture(None).unwrap());

                let canvas = recorder.begin_recording(grid_rect, None);
                canvas.clear(Color::from_argb(0, 255, 255, 255));
                for line_fragment in line_fragments.into_iter() {
                    let LineFragment {
                        text,
                        window_left,
                        width,
                        style,
                    } = line_fragment;
                    let grid_position = (window_left, 0);

                    grid_renderer.draw_foreground(canvas, text, grid_position, width, &style);
                }
                let foreground_picture = recorder.finish_recording_as_picture(None).unwrap();

                self.lines[line_index] = Some(Line {
                    background_picture,
                    foreground_picture,
                    has_transparency,
                });
            }
            WindowDrawCommand::Scroll {
                top,
                bottom,
                left,
                right,
                rows,
                cols,
            } => {
                tracy_zone!("scroll_cmd", 0);
                if top == 0
                    && bottom == self.grid_size.height
                    && left == 0
                    && right == self.grid_size.width
                    && cols == 0
                {
                    let mut scroll_offset = self.scroll_animation.position;
                    self.top_index += rows as isize;
                    let minmax = self.lines.len() - self.grid_size.height as usize;
                    if rows.unsigned_abs() as usize > minmax {
                        // The scroll offset has to be reset when scrolling too far
                        scroll_offset = 0.0;
                    } else {
                        scroll_offset -= rows as f32;
                        // And even when scrolling in steps, we can't let it drift too far, since the
                        // buffer size is limited
                        scroll_offset = scroll_offset.clamp(-(minmax as f32), minmax as f32);
                    }
                    self.scroll_animation.position = scroll_offset;
                }
            }
            WindowDrawCommand::Clear => {
                tracy_zone!("clear_cmd", 0);
                self.top_index = 0;
                self.scroll_animation.reset();
                self.current_surface.surface = build_window_surface_with_grid_size(
                    self.current_surface.surface.canvas(),
                    grid_renderer,
                    self.grid_size,
                );
            }
            WindowDrawCommand::Show => {
                tracy_zone!("show_cmd", 0);
                if self.hidden {
                    self.hidden = false;
                    self.position_t = 2.0; // We don't want to animate since the window is becoming visible,
                                           // so we set t to 2.0 to stop animations.
                    self.grid_start_position = self.grid_destination;
                    self.scroll_animation.reset();
                }
            }
            WindowDrawCommand::Hide => {
                tracy_zone!("hide_cmd", 0);
                self.hidden = true;
            }
            WindowDrawCommand::Viewport { .. } => {}
            _ => {}
        };
    }
}
