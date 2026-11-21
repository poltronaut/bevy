//! Shadow map slots and retained shadow maps for point and spot lights.
//!
//! Two things live here, both bookkeeping for `prepare_lights`:
//!
//! - [`ShadowMapSlots`] gives every shadowed point or spot light a *slot* in
//!   the shadow map arrays and lets it keep that slot for as long as it stays
//!   shadowed, whatever the other lights do. A point light's slot is a cube
//!   (six layers) of the cube array; a spot light's is one layer of the
//!   "directional" array, after the cascades. The lighting shader reads the
//!   slot from the light's data (the high bits of its `flags`), not from the
//!   light's place in the sorted list. The arrays are allocated for
//!   [`ShadowMapCapacity`](bevy_light::ShadowMapCapacity) slots or the highest
//!   slot in use, whichever is more.
//! - [`RetainedShadowMaps`] decides, per light with a
//!   [`RetainShadowMap`](bevy_light::RetainShadowMap), whether its map is
//!   rendered or kept this frame: it is kept once a render of it was complete
//!   (the *settled* rule), and as long as what it was rendered with (the
//!   *key*: its slot, pose, range, angles, near plane, render layers and
//!   generation) is the same.
//!
//! [`RetainedShadowMapVerification`] is a dev switch that proves a kept map
//! equals a fresh render, by reading both back and comparing their bytes.

use super::light::{
    ExtractedPointLight, PendingShadowQueues, SpecializedShadowMaterialPipelineCache,
};

use bevy_camera::visibility::RenderLayers;
use bevy_core_pipeline::schedule::{Core3d, RootNonCameraView};
use bevy_ecs::{
    entity::{EntityHashMap, EntityHashSet},
    prelude::*,
    schedule::ScheduleLabel,
};
use bevy_log::warn;
use bevy_render::{
    render_resource::*,
    renderer::{RenderDevice, RenderQueue},
    sync_world::MainEntity,
    view::RetainedViewEntity,
};
use bevy_transform::components::GlobalTransform;

/// The shadow slot of a point or spot light is stored in the bits of a clustered
/// light's `flags` from this one up. Must match `POINT_LIGHT_SHADOW_SLOT_SHIFT`
/// in `mesh_view_types.wgsl`.
pub(crate) const SHADOW_SLOT_SHIFT: u32 = 16;

/// Slots for the shadow maps of one kind of light (point or spot).
#[derive(Default)]
pub(crate) struct SlotTable {
    slots: EntityHashMap<u32>,
    /// One more than the highest slot ever handed out since the table last
    /// reset (see [`SlotTable::assign`]).
    high_water: usize,
    // Scratch.
    present: EntityHashSet,
    occupied: Vec<bool>,
}

impl SlotTable {
    /// Gives each of `lights` (the shadowed lights of this kind, in their sorted
    /// order) a slot.
    ///
    /// With `keep` off, the slots are the lights' positions in `lights` and
    /// nothing is remembered: the same as numbering the lights afresh each
    /// frame. With `keep` on, a light that has a slot keeps it, a light that
    /// left the list frees its slot, and a new light takes the lowest free
    /// slot; the table only grows until `keep` goes off.
    pub(crate) fn assign(&mut self, lights: &[Entity], keep: bool) {
        if !keep {
            self.slots.clear();
            self.slots
                .extend(lights.iter().enumerate().map(|(i, &e)| (e, i as u32)));
            self.high_water = lights.len();
            return;
        }

        self.present.clear();
        self.present.extend(lights.iter().copied());
        let present = &self.present;
        self.slots.retain(|entity, _| present.contains(entity));

        self.occupied.clear();
        self.occupied
            .resize(self.high_water.max(lights.len()), false);
        for &slot in self.slots.values() {
            self.occupied[slot as usize] = true;
        }
        let mut lowest_free = 0;
        for &light in lights {
            if self.slots.contains_key(&light) {
                continue;
            }
            while self.occupied.get(lowest_free).copied().unwrap_or(false) {
                lowest_free += 1;
            }
            if lowest_free >= self.occupied.len() {
                self.occupied.push(false);
            }
            self.occupied[lowest_free] = true;
            self.slots.insert(light, lowest_free as u32);
        }
        let highest_used = self.slots.values().map(|&s| s as usize + 1).max();
        self.high_water = self.high_water.max(highest_used.unwrap_or(0));
    }

    /// The slot of `light`, if it is shadowed.
    pub(crate) fn get(&self, light: Entity) -> Option<u32> {
        self.slots.get(&light).copied()
    }

    /// How many slots the array holds: the ones in use or reserved by the
    /// high-water mark, or `capacity`, whichever is more, but at most `limit`.
    pub(crate) fn slot_count(&self, capacity: usize, limit: usize) -> usize {
        self.high_water.max(capacity).min(limit)
    }
}

/// Which slot each shadowed point and spot light has in the shadow map arrays.
///
/// Render world bookkeeping for `prepare_lights`; public so tooling can read
/// the layout.
#[derive(Resource, Default)]
pub struct ShadowMapSlots {
    pub(crate) points: SlotTable,
    pub(crate) spots: SlotTable,
    // Scratch for `assign`.
    shadowed_points: Vec<Entity>,
    shadowed_spots: Vec<Entity>,
}

impl ShadowMapSlots {
    /// Starts handing out slots for a frame: push the shadowed lights in sorted
    /// order, then [`finish_assign`](Self::finish_assign).
    pub(crate) fn begin_assign(&mut self) {
        self.shadowed_points.clear();
        self.shadowed_spots.clear();
    }

    pub(crate) fn push(&mut self, light: Entity, is_spot: bool) {
        if is_spot {
            self.shadowed_spots.push(light);
        } else {
            self.shadowed_points.push(light);
        }
    }

    /// Assigns the slots of the lights pushed; `keep` is whether any of them
    /// asks to keep its map (see [`SlotTable::assign`]).
    pub(crate) fn finish_assign(&mut self, keep: bool) {
        self.points.assign(&self.shadowed_points, keep);
        self.spots.assign(&self.shadowed_spots, keep);
    }

    /// The cube slot of a shadowed point light (render world entity).
    pub fn point_slot(&self, light: Entity) -> Option<u32> {
        self.points.get(light)
    }

    /// The layer slot of a shadowed spot light (render world entity), counted
    /// from the first layer after the cascades.
    pub fn spot_slot(&self, light: Entity) -> Option<u32> {
        self.spots.get(light)
    }
}

/// A shadow map kept by [`RetainShadowMap`](bevy_light::RetainShadowMap) is only
/// still valid while all of this is unchanged. Casters are not part of it: the
/// renderer can't see them change, which is what the generation is for.
#[derive(Clone, PartialEq)]
struct RetainedShadowMapKey {
    /// The depth texture array the map lives in. A different texture (the
    /// arrays were reallocated) means the contents are gone.
    texture: TextureId,
    /// The first layer of the light's map in that texture.
    base_layer: u32,
    /// The game's request for one more render.
    generation: u32,
    transform: GlobalTransform,
    range: f32,
    shadow_map_near_z: f32,
    spot_light_angles: Option<(f32, f32)>,
    render_layers: RenderLayers,
}

/// A light's map between frames, while it has a
/// [`RetainShadowMap`](bevy_light::RetainShadowMap).
struct KeptMap {
    key: RetainedShadowMapKey,
    /// Whether the light's shadow views are currently taken out of the render
    /// graph (they have no `RootNonCameraView`).
    suppressed: bool,
    /// Whether a render with this key was complete (no queue pending, every
    /// pipeline ready): only then is the map trusted.
    settled: bool,
    /// Rendered this frame and not judged yet.
    to_settle: bool,
    main_entity: MainEntity,
    view_count: u32,
}

/// Where a light's map was last frame, and what it was rendered with if the
/// light is marked.
struct LightPlacement {
    texture: TextureId,
    base_layer: u32,
    kept: Option<KeptMap>,
}

/// Everything [`RetainedShadowMaps::update`] needs to know about one shadowed
/// light.
pub(crate) struct LightShadowInfo<'a> {
    pub entity: Entity,
    pub main_entity: MainEntity,
    pub light: &'a ExtractedPointLight,
    pub render_layers: &'a RenderLayers,
    /// The depth texture array the light's map is in this frame.
    pub texture: TextureId,
    /// The first layer of the light's map in it.
    pub base_layer: u32,
    /// Shadow views: 6 for a point light, 1 for a spot light.
    pub view_count: u32,
    /// Whether the light's shadow view entities exist.
    pub has_views: bool,
}

/// What [`RetainedShadowMaps::update`] decided for one light this frame.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ShadowMapRetention {
    /// The shadow map is rendered this frame.
    Render {
        /// The light's map moved (another texture or layer) since it was last
        /// seen, so its views must be rebuilt.
        moved: bool,
        /// The light's views were suppressed and must be put back.
        resume: bool,
    },
    /// The shadow map keeps the contents of the frame it was last rendered in.
    Retain {
        /// First frame of retention: the views must be taken out of the
        /// render graph.
        start: bool,
    },
    /// The shadow map would be kept, but a verification asked for a fresh
    /// render to compare it with: render it like [`ShadowMapRetention::Render`]
    /// and read the layers back.
    Verify {
        /// The light's views were suppressed and must be put back.
        resume: bool,
    },
}

/// The point and spot light shadow maps kept between frames by
/// [`RetainShadowMap`](bevy_light::RetainShadowMap), and what they were rendered
/// with.
///
/// This is render world bookkeeping for `prepare_lights`; it is public so a
/// diagnostics tool or a game's re-render budget can read how many marked lights
/// were kept and how many were rendered last frame.
#[derive(Resource, Default)]
pub struct RetainedShadowMaps {
    lights: EntityHashMap<LightPlacement>,
    seen: EntityHashSet,
    retained_lights: usize,
    rendered_lights: usize,
}

impl RetainedShadowMaps {
    /// The number of marked lights whose shadow map was kept (not rendered) in
    /// the last frame `prepare_lights` ran.
    pub fn retained_lights(&self) -> usize {
        self.retained_lights
    }

    /// The number of marked lights whose shadow map was rendered in the last
    /// frame `prepare_lights` ran: a first render, one asked for by a changed
    /// [`generation`](bevy_light::RetainShadowMap::generation) or by a change of the
    /// light or of the arrays, or one repeated because the last was not
    /// complete yet. A game's re-render budget counts these (each costs a full
    /// shadow pass of the light's views, not only the depth draw: culling,
    /// specialization and queueing too).
    pub fn rendered_lights(&self) -> usize {
        self.rendered_lights
    }

    pub(crate) fn begin_frame(&mut self) {
        self.seen.clear();
        self.retained_lights = 0;
        self.rendered_lights = 0;
        for state in self.lights.values_mut() {
            if let Some(kept) = state.kept.as_mut() {
                kept.to_settle = false;
            }
        }
    }

    /// Forgets lights that `prepare_lights` didn't visit this frame (their
    /// views are despawned with their extracted light).
    pub(crate) fn end_frame(&mut self) {
        let seen = &self.seen;
        self.lights.retain(|entity, _| seen.contains(entity));
    }

    /// Forgets a light (its shadow views are gone or its map is rendered
    /// from scratch), returning whether its views were suppressed.
    pub(crate) fn forget(&mut self, entity: Entity) -> bool {
        self.seen.insert(entity);
        self.lights
            .remove(&entity)
            .and_then(|state| state.kept)
            .is_some_and(|kept| kept.suppressed)
    }

    /// Decides what to do with one shadowed light this frame.
    ///
    /// `verify_budget` is how many kept maps may still be asked for a
    /// verification render this frame; a light that would be kept uses one.
    pub(crate) fn update(
        &mut self,
        info: &LightShadowInfo,
        verify_budget: &mut u32,
    ) -> ShadowMapRetention {
        self.seen.insert(info.entity);
        let placement = (info.texture, info.base_layer);

        let previous = self.lights.remove(&info.entity);
        let moved = info.has_views
            && previous
                .as_ref()
                .is_some_and(|p| (p.texture, p.base_layer) != placement);
        let previous_kept = previous.and_then(|p| p.kept);
        let resume = previous_kept.as_ref().is_some_and(|kept| kept.suppressed);

        let generation = info.light.retain_shadow_map.filter(|_| info.has_views);
        let Some(generation) = generation else {
            self.lights.insert(
                info.entity,
                LightPlacement {
                    texture: info.texture,
                    base_layer: info.base_layer,
                    kept: None,
                },
            );
            return ShadowMapRetention::Render { moved, resume };
        };

        let key = RetainedShadowMapKey {
            texture: info.texture,
            base_layer: info.base_layer,
            generation,
            transform: info.light.transform,
            range: info.light.range,
            shadow_map_near_z: info.light.shadow_map_near_z,
            spot_light_angles: info.light.spot_light_angles,
            render_layers: info.render_layers.clone(),
        };
        let mut kept = match previous_kept {
            Some(kept) if kept.key == key => kept,
            _ => KeptMap {
                key,
                suppressed: false,
                settled: false,
                to_settle: false,
                main_entity: info.main_entity,
                view_count: info.view_count,
            },
        };

        let outcome = if kept.settled {
            if *verify_budget > 0 {
                *verify_budget -= 1;
                kept.suppressed = false;
                ShadowMapRetention::Verify { resume }
            } else {
                let start = !kept.suppressed;
                kept.suppressed = true;
                self.retained_lights += 1;
                ShadowMapRetention::Retain { start }
            }
        } else {
            kept.suppressed = false;
            kept.to_settle = true;
            self.rendered_lights += 1;
            ShadowMapRetention::Render { moved, resume }
        };
        self.lights.insert(
            info.entity,
            LightPlacement {
                texture: info.texture,
                base_layer: info.base_layer,
                kept: Some(kept),
            },
        );
        outcome
    }

    /// Judges the maps rendered this frame: `is_settled(main_entity,
    /// view_count)` says whether all of the light's shadow views were rendered
    /// completely. A map is kept from the next frame on once it was.
    pub(crate) fn settle(&mut self, mut is_settled: impl FnMut(MainEntity, u32) -> bool) {
        for state in self.lights.values_mut() {
            if let Some(kept) = state.kept.as_mut()
                && kept.to_settle
            {
                kept.to_settle = false;
                kept.settled = is_settled(kept.main_entity, kept.view_count);
            }
        }
    }
}

/// Takes a light's shadow views out of the render graph, so they run no
/// `Core3d` schedule, or puts them back.
fn set_shadow_views_suppressed(commands: &mut Commands, views: &[Entity], suppressed: bool) {
    for view in views {
        let Ok(mut view) = commands.get_entity(*view) else {
            continue;
        };
        if suppressed {
            view.remove::<RootNonCameraView>();
        } else {
            view.insert(RootNonCameraView(Core3d.intern()));
        }
    }
}

/// Carries out what [`RetainedShadowMaps::update`] decided on a light's shadow
/// views: takes them out of the render graph or puts them back. Returns `None`
/// if the light's map is kept (nothing else to do for it this frame), or
/// whether its map moved if it is rendered.
///
/// `layers` is what a verification reads back.
pub(crate) fn apply_shadow_map_retention(
    commands: &mut Commands,
    views: &[Entity],
    retention: ShadowMapRetention,
    verification: &mut RetainedShadowMapVerification,
    layers: VerifyLayers,
) -> Option<bool> {
    match retention {
        // Keep the map: no views run, no phases are prepared, and the light is
        // left out of `live_shadow_mapping_lights` so its phases and caches are
        // dropped until it renders again.
        ShadowMapRetention::Retain { start } => {
            if start {
                set_shadow_views_suppressed(commands, views, true);
            }
            None
        }
        ShadowMapRetention::Render { moved, resume } => {
            if resume {
                set_shadow_views_suppressed(commands, views, false);
            }
            Some(moved)
        }
        ShadowMapRetention::Verify { resume } => {
            if resume {
                set_shadow_views_suppressed(commands, views, false);
            }
            verification.request(layers);
            Some(false)
        }
    }
}

/// After the shadow views were queued: tells [`RetainedShadowMaps`] which of the
/// lights that rendered this frame rendered completely. A light's views are
/// complete when none of them has a caster whose mesh or material wasn't ready
/// (`PendingShadowQueues`) and every pipeline they use is compiled.
pub(crate) fn settle_retained_shadow_maps(
    mut retained: ResMut<RetainedShadowMaps>,
    pending_shadow_queues: Res<PendingShadowQueues>,
    pipelines: Res<SpecializedShadowMaterialPipelineCache>,
    pipeline_cache: Res<PipelineCache>,
) {
    retained.settle(|main_entity, view_count| {
        (0..view_count).all(|face| {
            let view = RetainedViewEntity::new(main_entity, None, face);
            // No entry: the views weren't specialized yet this frame.
            let Some(queues) = pending_shadow_queues.get(&view) else {
                return false;
            };
            queues.current_frame.is_empty()
                && pipelines.get(&view).is_none_or(|view_pipelines| {
                    view_pipelines
                        .values()
                        .all(|(id, _)| pipeline_cache.get_render_pipeline(*id).is_some())
                })
        })
    });
}

/// A dev switch that proves kept shadow maps equal fresh renders.
///
/// With [`enabled`](Self::enabled) set (insert the resource in the render app
/// before the first frame; changing it later reallocates the shadow map
/// arrays), every [`interval`](Self::interval) frames up to
/// [`lights_per_check`](Self::lights_per_check) kept maps are rendered fresh
/// instead of kept. Their layers are copied out before the shadow pass (what
/// was kept) and after it (what a fresh render gives) and the bytes compared on
/// the CPU, waiting for the GPU: a frame hitch, so only for runs that check.
/// The results add up in [`stats`](Self::stats).
#[derive(Resource, Debug, Clone)]
pub struct RetainedShadowMapVerification {
    /// Whether verification runs.
    pub enabled: bool,
    /// A check happens every this many frames (at least 1).
    pub interval: u32,
    /// How many kept maps a check compares.
    pub lights_per_check: u32,
    /// What the checks found so far.
    pub stats: ShadowMapVerificationStats,
    frame: u32,
    requests: Vec<VerifyRequest>,
}

impl Default for RetainedShadowMapVerification {
    fn default() -> Self {
        Self {
            enabled: false,
            interval: 1,
            lights_per_check: 1,
            stats: Default::default(),
            frame: 0,
            requests: Vec::new(),
        }
    }
}

/// The results of [`RetainedShadowMapVerification`].
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ShadowMapVerificationStats {
    /// Kept maps compared with a fresh render.
    pub maps_checked: u64,
    /// Layers compared (6 a point light, 1 a spot light).
    pub layers_compared: u64,
    /// Layers that differed in at least one texel.
    pub layers_differing: u64,
    /// Texels that differed, over all layers compared.
    pub texels_differing: u64,
    /// The largest difference in stored depth between a kept and a fresh texel.
    pub max_depth_delta: f32,
}

/// The layers of a kept map that a verification reads back.
pub(crate) struct VerifyLayers<'a> {
    /// The depth texture array the map is in.
    pub texture: &'a Texture,
    /// The first layer of the map.
    pub base_layer: u32,
    /// How many layers: 6 for a point light, 1 for a spot light.
    pub layers: u32,
    /// The width and height of a layer.
    pub size: u32,
    /// The light, for the log.
    pub light: MainEntity,
}

#[derive(Debug, Clone)]
struct VerifyRequest {
    texture: Texture,
    base_layer: u32,
    layers: u32,
    size: u32,
    light: MainEntity,
    kept: Option<Buffer>,
}

impl RetainedShadowMapVerification {
    /// Starts a frame, returning how many kept maps may be verified in it.
    pub(crate) fn begin_frame(&mut self) -> u32 {
        self.requests.clear();
        if !self.enabled {
            return 0;
        }
        self.frame = self.frame.wrapping_add(1);
        if self.frame.is_multiple_of(self.interval.max(1)) {
            self.lights_per_check
        } else {
            0
        }
    }

    /// Asks for the layers of a kept map to be copied before this frame's shadow
    /// pass and compared with the fresh render after it.
    pub(crate) fn request(&mut self, target: VerifyLayers) {
        self.requests.push(VerifyRequest {
            texture: target.texture.clone(),
            base_layer: target.base_layer,
            layers: target.layers,
            size: target.size,
            light: target.light,
            kept: None,
        });
    }
}

fn depth_layers_to_buffer(encoder: &mut CommandEncoder, request: &VerifyRequest, buffer: &Buffer) {
    let bytes_per_row = RenderDevice::align_copy_bytes_per_row(request.size as usize * 4) as u32;
    let layer_bytes = bytes_per_row as u64 * request.size as u64;
    for layer in 0..request.layers {
        encoder.copy_texture_to_buffer(
            TexelCopyTextureInfo {
                texture: &request.texture,
                mip_level: 0,
                origin: Origin3d {
                    x: 0,
                    y: 0,
                    z: request.base_layer + layer,
                },
                aspect: TextureAspect::DepthOnly,
            },
            TexelCopyBufferInfo {
                buffer,
                layout: TexelCopyBufferLayout {
                    offset: layer as u64 * layer_bytes,
                    bytes_per_row: Some(bytes_per_row),
                    rows_per_image: Some(request.size),
                },
            },
            Extent3d {
                width: request.size,
                height: request.size,
                depth_or_array_layers: 1,
            },
        );
    }
}

fn readback_buffer(device: &RenderDevice, request: &VerifyRequest, label: &'static str) -> Buffer {
    let bytes_per_row = RenderDevice::align_copy_bytes_per_row(request.size as usize * 4) as u64;
    device.create_buffer(&BufferDescriptor {
        label: Some(label),
        size: bytes_per_row * request.size as u64 * request.layers as u64,
        usage: BufferUsages::COPY_DST | BufferUsages::MAP_READ,
        mapped_at_creation: false,
    })
}

/// Before the shadow pass: copies out the layers of the maps that a
/// verification will compare with a fresh render.
pub(crate) fn snapshot_kept_shadow_layers(
    mut verification: ResMut<RetainedShadowMapVerification>,
    device: Res<RenderDevice>,
    queue: Res<RenderQueue>,
) {
    if verification.requests.is_empty() {
        return;
    }
    let mut encoder = device.create_command_encoder(&CommandEncoderDescriptor {
        label: Some("retained_shadow_map_snapshot"),
    });
    for request in &mut verification.requests {
        let buffer = readback_buffer(&device, request, "retained_shadow_map_kept");
        depth_layers_to_buffer(&mut encoder, request, &buffer);
        request.kept = Some(buffer);
    }
    queue.submit([encoder.finish()]);
}

fn read_buffer(device: &RenderDevice, buffer: &Buffer) -> Vec<u8> {
    let slice = buffer.slice(..);
    slice.map_async(MapMode::Read, |result| {
        if let Err(err) = result {
            warn!("retained shadow map verification couldn't map a buffer: {err}");
        }
    });
    if let Err(err) = device.poll(PollType::wait_indefinitely()) {
        warn!("retained shadow map verification couldn't wait for the GPU: {err}");
    }
    let data = slice.get_mapped_range().to_vec();
    buffer.unmap();
    data
}

/// After the shadow pass: copies out the same layers, as freshly rendered, and
/// compares them with the kept ones, byte for byte.
pub(crate) fn compare_verified_shadow_layers(
    mut verification: ResMut<RetainedShadowMapVerification>,
    device: Res<RenderDevice>,
    queue: Res<RenderQueue>,
) {
    if verification.requests.is_empty() {
        return;
    }
    let requests = core::mem::take(&mut verification.requests);
    let mut encoder = device.create_command_encoder(&CommandEncoderDescriptor {
        label: Some("retained_shadow_map_fresh"),
    });
    let fresh: Vec<Buffer> = requests
        .iter()
        .map(|request| {
            let buffer = readback_buffer(&device, request, "retained_shadow_map_fresh");
            depth_layers_to_buffer(&mut encoder, request, &buffer);
            buffer
        })
        .collect();
    queue.submit([encoder.finish()]);

    for (request, fresh) in requests.iter().zip(&fresh) {
        let Some(kept) = &request.kept else {
            continue;
        };
        let (kept, fresh) = (read_buffer(&device, kept), read_buffer(&device, fresh));
        let bytes_per_row = RenderDevice::align_copy_bytes_per_row(request.size as usize * 4);
        let result = compare_depth_layers(&kept, &fresh, request.size as usize, bytes_per_row);
        if result.layers_differing > 0 {
            warn!(
                "a kept shadow map differs from a fresh render: light {:?}, layers {}..{} of {}, {} differ, {} texels, largest depth delta {:e}",
                request.light,
                request.base_layer,
                request.base_layer + request.layers,
                request.size,
                result.layers_differing,
                result.texels_differing,
                result.max_depth_delta
            );
        }
        let stats = &mut verification.stats;
        stats.maps_checked += 1;
        stats.layers_compared += request.layers as u64;
        stats.layers_differing += result.layers_differing as u64;
        stats.texels_differing += result.texels_differing;
        stats.max_depth_delta = stats.max_depth_delta.max(result.max_depth_delta);
    }
}

#[derive(Debug, Default, PartialEq)]
struct LayerComparison {
    layers_differing: usize,
    texels_differing: u64,
    max_depth_delta: f32,
}

/// Compares two readbacks of depth layers laid out with `bytes_per_row` bytes
/// a row (rows of `size` 32 bit depths, then padding).
fn compare_depth_layers(
    kept: &[u8],
    fresh: &[u8],
    size: usize,
    bytes_per_row: usize,
) -> LayerComparison {
    let layer_bytes = bytes_per_row * size;
    let mut result = LayerComparison::default();
    if kept.len() != fresh.len() || layer_bytes == 0 {
        return result;
    }
    for (kept_layer, fresh_layer) in kept.chunks(layer_bytes).zip(fresh.chunks(layer_bytes)) {
        let mut differing = 0u64;
        for (kept_row, fresh_row) in kept_layer
            .chunks(bytes_per_row)
            .zip(fresh_layer.chunks(bytes_per_row))
        {
            for (k, f) in kept_row[..size * 4]
                .chunks_exact(4)
                .zip(fresh_row[..size * 4].chunks_exact(4))
            {
                if k != f {
                    differing += 1;
                    let (k, f) = (
                        f32::from_le_bytes([k[0], k[1], k[2], k[3]]),
                        f32::from_le_bytes([f[0], f[1], f[2], f[3]]),
                    );
                    result.max_depth_delta = result.max_depth_delta.max((k - f).abs());
                }
            }
        }
        if differing > 0 {
            result.layers_differing += 1;
            result.texels_differing += differing;
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy_color::LinearRgba;

    fn extracted(retain_shadow_map: Option<u32>) -> ExtractedPointLight {
        ExtractedPointLight {
            color: LinearRgba::WHITE,
            intensity: 1.0,
            range: 10.0,
            radius: 0.0,
            transform: GlobalTransform::from_xyz(1.0, 2.0, 3.0),
            shadow_maps_enabled: true,
            contact_shadows_enabled: false,
            shadow_depth_bias: 0.0,
            shadow_normal_bias: 0.0,
            shadow_map_near_z: 0.1,
            spot_light_angles: None,
            volumetric: false,
            soft_shadows_enabled: false,
            affects_lightmapped_mesh_diffuse: true,
            retain_shadow_map,
        }
    }

    fn entity(n: u32) -> Entity {
        Entity::from_raw_u32(n).unwrap()
    }

    /// One frame for one light: what `prepare_lights` does with it, and then
    /// what the settle system does when `complete` says the render was
    /// complete.
    fn frame_with(
        maps: &mut RetainedShadowMaps,
        e: Entity,
        light: &ExtractedPointLight,
        placement: (TextureId, u32),
        complete: bool,
        verify_budget: &mut u32,
    ) -> ShadowMapRetention {
        maps.begin_frame();
        let retention = maps.update(
            &LightShadowInfo {
                entity: e,
                main_entity: MainEntity::from(e),
                light,
                render_layers: &RenderLayers::default(),
                texture: placement.0,
                base_layer: placement.1,
                view_count: 6,
                has_views: true,
            },
            verify_budget,
        );
        maps.settle(|_, _| complete);
        maps.end_frame();
        retention
    }

    fn frame(
        maps: &mut RetainedShadowMaps,
        e: Entity,
        light: &ExtractedPointLight,
        placement: (TextureId, u32),
    ) -> ShadowMapRetention {
        frame_with(maps, e, light, placement, true, &mut 0)
    }

    fn is_render(retention: &ShadowMapRetention) -> bool {
        matches!(retention, ShadowMapRetention::Render { .. })
    }

    #[test]
    fn a_marked_light_renders_once_then_keeps_its_map() {
        let (mut maps, e, texture) = (RetainedShadowMaps::default(), entity(7), TextureId::new());
        let light = extracted(Some(0));
        assert!(
            is_render(&frame(&mut maps, e, &light, (texture, 0))),
            "the first frame renders"
        );
        assert_eq!((maps.retained_lights(), maps.rendered_lights()), (0, 1));
        assert_eq!(
            frame(&mut maps, e, &light, (texture, 0)),
            ShadowMapRetention::Retain { start: true }
        );
        assert_eq!((maps.retained_lights(), maps.rendered_lights()), (1, 0));
        assert_eq!(
            frame(&mut maps, e, &light, (texture, 0)),
            ShadowMapRetention::Retain { start: false }
        );
    }

    #[test]
    fn an_unmarked_light_renders_every_frame() {
        let (mut maps, e, texture) = (RetainedShadowMaps::default(), entity(7), TextureId::new());
        let light = extracted(None);
        for _ in 0..3 {
            assert!(is_render(&frame(&mut maps, e, &light, (texture, 0))));
        }
        assert_eq!((maps.retained_lights(), maps.rendered_lights()), (0, 0));
    }

    #[test]
    fn a_map_is_kept_only_after_a_complete_render() {
        let (mut maps, e, texture) = (RetainedShadowMaps::default(), entity(7), TextureId::new());
        let light = extracted(Some(0));
        // Casters or pipelines still pending: every frame renders, and counts.
        for _ in 0..3 {
            let retention = frame_with(&mut maps, e, &light, (texture, 0), false, &mut 0);
            assert!(is_render(&retention));
            assert_eq!(maps.rendered_lights(), 1);
            assert_eq!(maps.retained_lights(), 0);
        }
        // The first complete render settles it...
        assert!(is_render(&frame(&mut maps, e, &light, (texture, 0))));
        // ...and the next frame keeps it.
        assert_eq!(
            frame(&mut maps, e, &light, (texture, 0)),
            ShadowMapRetention::Retain { start: true }
        );
    }

    #[test]
    fn a_render_that_was_not_judged_does_not_settle() {
        let (mut maps, e, texture) = (RetainedShadowMaps::default(), entity(7), TextureId::new());
        let light = extracted(Some(0));
        // `settle` never ran (no shadow views were specialized): not trusted.
        for _ in 0..3 {
            maps.begin_frame();
            let retention = maps.update(
                &LightShadowInfo {
                    entity: e,
                    main_entity: MainEntity::from(e),
                    light: &light,
                    render_layers: &RenderLayers::default(),
                    texture,
                    base_layer: 0,
                    view_count: 6,
                    has_views: true,
                },
                &mut 0,
            );
            maps.end_frame();
            assert!(is_render(&retention));
        }
    }

    #[test]
    fn a_light_that_changed_renders_again_and_puts_its_views_back() {
        let (mut maps, e, texture) = (RetainedShadowMaps::default(), entity(7), TextureId::new());
        let mut light = extracted(Some(0));
        frame(&mut maps, e, &light, (texture, 0));
        frame(&mut maps, e, &light, (texture, 0));
        light.transform = GlobalTransform::from_xyz(1.0, 2.5, 3.0);
        assert_eq!(
            frame(&mut maps, e, &light, (texture, 0)),
            ShadowMapRetention::Render {
                moved: false,
                resume: true
            },
            "same place in the texture, views were suppressed"
        );
        assert_eq!(
            frame(&mut maps, e, &light, (texture, 0)),
            ShadowMapRetention::Retain { start: true },
            "and is kept again from the new pose, once settled"
        );
    }

    #[test]
    fn bumping_the_generation_renders_once() {
        let (mut maps, e, texture) = (RetainedShadowMaps::default(), entity(7), TextureId::new());
        frame(&mut maps, e, &extracted(Some(0)), (texture, 0));
        frame(&mut maps, e, &extracted(Some(0)), (texture, 0));
        for _ in 0..3 {
            assert!(matches!(
                frame(&mut maps, e, &extracted(Some(0)), (texture, 0)),
                ShadowMapRetention::Retain { .. }
            ));
        }
        // One more render is asked for, and costs exactly one frame.
        assert_eq!(
            frame(&mut maps, e, &extracted(Some(1)), (texture, 0)),
            ShadowMapRetention::Render {
                moved: false,
                resume: true
            }
        );
        assert_eq!(maps.rendered_lights(), 1);
        assert!(matches!(
            frame(&mut maps, e, &extracted(Some(1)), (texture, 0)),
            ShadowMapRetention::Retain { start: true }
        ));
        assert!(matches!(
            frame(&mut maps, e, &extracted(Some(1)), (texture, 0)),
            ShadowMapRetention::Retain { start: false }
        ));
        // Wrapping around is a change like any other.
        assert!(is_render(&frame(
            &mut maps,
            e,
            &extracted(Some(u32::MAX)),
            (texture, 0)
        )));
    }

    #[test]
    fn a_map_that_moved_in_its_texture_is_rendered_again() {
        let (mut maps, e) = (RetainedShadowMaps::default(), entity(7));
        let (first, second) = (TextureId::new(), TextureId::new());
        let light = extracted(Some(0));
        frame(&mut maps, e, &light, (first, 6));
        frame(&mut maps, e, &light, (first, 6));
        assert_eq!(
            frame(&mut maps, e, &light, (first, 12)),
            ShadowMapRetention::Render {
                moved: true,
                resume: true
            }
        );
        frame(&mut maps, e, &light, (first, 12));
        // The array was reallocated: its contents are gone.
        assert_eq!(
            frame(&mut maps, e, &light, (second, 12)),
            ShadowMapRetention::Render {
                moved: true,
                resume: true
            }
        );
    }

    #[test]
    fn an_unmarked_light_whose_map_moved_has_its_views_rebuilt() {
        let (mut maps, e) = (RetainedShadowMaps::default(), entity(7));
        let (first, second) = (TextureId::new(), TextureId::new());
        let light = extracted(None);
        frame(&mut maps, e, &light, (first, 0));
        assert_eq!(
            frame(&mut maps, e, &light, (first, 0)),
            ShadowMapRetention::Render {
                moved: false,
                resume: false
            }
        );
        assert_eq!(
            frame(&mut maps, e, &light, (second, 6)),
            ShadowMapRetention::Render {
                moved: true,
                resume: false
            }
        );
    }

    #[test]
    fn dropping_the_mark_or_the_shadow_renders_again() {
        let (mut maps, e, texture) = (RetainedShadowMaps::default(), entity(7), TextureId::new());
        frame(&mut maps, e, &extracted(Some(0)), (texture, 0));
        frame(&mut maps, e, &extracted(Some(0)), (texture, 0));
        assert_eq!(
            frame(&mut maps, e, &extracted(None), (texture, 0)),
            ShadowMapRetention::Render {
                moved: false,
                resume: true
            }
        );
        // Marked again: it renders first, it isn't trusted from before.
        assert!(is_render(&frame(
            &mut maps,
            e,
            &extracted(Some(0)),
            (texture, 0)
        )));

        frame(&mut maps, e, &extracted(Some(0)), (texture, 0));
        maps.begin_frame();
        assert!(
            maps.forget(e),
            "a light that lost its shadow had its views suppressed"
        );
        maps.end_frame();
        assert!(is_render(&frame(
            &mut maps,
            e,
            &extracted(Some(0)),
            (texture, 0)
        )));
    }

    #[test]
    fn a_light_that_left_the_frame_is_forgotten() {
        let (mut maps, e, texture) = (RetainedShadowMaps::default(), entity(7), TextureId::new());
        frame(&mut maps, e, &extracted(Some(0)), (texture, 0));
        frame(&mut maps, e, &extracted(Some(0)), (texture, 0));
        maps.begin_frame();
        maps.end_frame();
        assert!(
            is_render(&frame(&mut maps, e, &extracted(Some(0)), (texture, 0))),
            "its views were despawned with it"
        );
    }

    #[test]
    fn a_light_without_views_can_not_be_kept() {
        let (mut maps, e, texture) = (RetainedShadowMaps::default(), entity(7), TextureId::new());
        let light = extracted(Some(0));
        frame(&mut maps, e, &light, (texture, 0));
        maps.begin_frame();
        assert!(is_render(&maps.update(
            &LightShadowInfo {
                entity: e,
                main_entity: MainEntity::from(e),
                light: &light,
                render_layers: &RenderLayers::default(),
                texture,
                base_layer: 0,
                view_count: 6,
                has_views: false,
            },
            &mut 0
        )));
    }

    #[test]
    fn a_verification_renders_a_kept_map_fresh_and_then_keeps_it_again() {
        let (mut maps, e, texture) = (RetainedShadowMaps::default(), entity(7), TextureId::new());
        let light = extracted(Some(0));
        frame(&mut maps, e, &light, (texture, 0));
        assert!(matches!(
            frame(&mut maps, e, &light, (texture, 0)),
            ShadowMapRetention::Retain { start: true }
        ));
        let mut budget = 1;
        assert_eq!(
            frame_with(&mut maps, e, &light, (texture, 0), true, &mut budget),
            ShadowMapRetention::Verify { resume: true }
        );
        assert_eq!(budget, 0, "one verification used");
        assert_eq!(
            (maps.retained_lights(), maps.rendered_lights()),
            (0, 0),
            "a verification render is not a re-render"
        );
        assert_eq!(
            frame(&mut maps, e, &light, (texture, 0)),
            ShadowMapRetention::Retain { start: true },
            "the views were put back, so retention starts again"
        );
        // A light that is not kept yet doesn't use the budget.
        let (other, mut budget) = (entity(8), 1);
        assert!(is_render(&frame_with(
            &mut maps,
            other,
            &extracted(Some(0)),
            (texture, 6),
            true,
            &mut budget
        )));
        assert_eq!(budget, 1);
    }

    #[test]
    fn slots_follow_the_sorted_order_while_nothing_is_kept() {
        let mut table = SlotTable::default();
        let lights = [entity(1), entity(2), entity(3)];
        table.assign(&lights, false);
        assert_eq!(
            lights.map(|l| table.get(l).unwrap()),
            [0, 1, 2],
            "numbered afresh"
        );
        // A light that sorts first shifts the others, as it always did.
        table.assign(&[entity(0), entity(1), entity(2), entity(3)], false);
        assert_eq!(table.get(entity(3)), Some(3));
        assert_eq!(table.slot_count(0, 100), 4);
        // And the arrays shrink when lights leave.
        table.assign(&[entity(1)], false);
        assert_eq!(table.slot_count(0, 100), 1);
        assert_eq!(table.get(entity(2)), None);
    }

    #[test]
    fn a_light_keeps_its_slot_when_other_lights_come_and_go() {
        let mut table = SlotTable::default();
        table.assign(&[entity(5), entity(6)], true);
        let (five, six) = (table.get(entity(5)).unwrap(), table.get(entity(6)).unwrap());
        // A light with a lower sort key appears: nothing moves, it takes a new slot.
        table.assign(&[entity(1), entity(5), entity(6)], true);
        assert_eq!(table.get(entity(5)), Some(five));
        assert_eq!(table.get(entity(6)), Some(six));
        assert_eq!(table.get(entity(1)), Some(2));
        // It leaves again: still nothing moves.
        table.assign(&[entity(5), entity(6)], true);
        assert_eq!(table.get(entity(5)), Some(five));
        assert_eq!(table.get(entity(6)), Some(six));
        assert_eq!(table.get(entity(1)), None);
    }

    #[test]
    fn slots_are_reused_after_a_light_leaves() {
        let mut table = SlotTable::default();
        table.assign(&[entity(1), entity(2), entity(3)], true);
        assert_eq!(table.get(entity(2)), Some(1));
        table.assign(&[entity(1), entity(3)], true);
        // A new light takes the hole, not a new slot at the end.
        table.assign(&[entity(1), entity(3), entity(9)], true);
        assert_eq!(table.get(entity(9)), Some(1));
        assert_eq!(table.slot_count(0, 100), 3, "the array didn't grow");
        // Two arrive at once: lowest free slots in the given order.
        table.assign(
            &[entity(1), entity(3), entity(9), entity(10), entity(11)],
            true,
        );
        assert_eq!(table.get(entity(10)), Some(3));
        assert_eq!(table.get(entity(11)), Some(4));
    }

    #[test]
    fn capacity_is_a_floor_and_growth_never_shrinks_while_kept() {
        let mut table = SlotTable::default();
        table.assign(&[entity(1)], true);
        assert_eq!(table.slot_count(0, 100), 1, "default: what is needed");
        assert_eq!(table.slot_count(8, 100), 8, "capacity reserves room");
        assert_eq!(table.slot_count(8, 4), 4, "but not past the device limit");
        table.assign(&(1..=10).map(entity).collect::<Vec<_>>(), true);
        assert_eq!(
            table.slot_count(8, 100),
            10,
            "more lights than capacity: grow"
        );
        table.assign(&[entity(1)], true);
        assert_eq!(
            table.slot_count(8, 100),
            10,
            "lights leaving doesn't shrink it while slots are kept"
        );
        table.assign(&[entity(1)], false);
        assert_eq!(table.slot_count(0, 100), 1, "back to what is needed");
    }

    #[test]
    fn a_light_that_lost_its_shadow_frees_its_slot() {
        let mut table = SlotTable::default();
        table.assign(&[entity(1), entity(2)], true);
        // Entity 1 is no longer among the shadowed lights (budget flipped it).
        table.assign(&[entity(2)], true);
        assert_eq!(table.get(entity(1)), None);
        assert_eq!(table.get(entity(2)), Some(1));
        // Back again: it gets the free slot.
        table.assign(&[entity(1), entity(2)], true);
        assert_eq!(table.get(entity(1)), Some(0));
    }

    #[test]
    fn the_comparison_counts_texels_layers_and_the_largest_delta() {
        // 2x2 layers, 256-byte rows (as a copy to a buffer pads them).
        let (size, row) = (2, 256);
        let layer = |texels: [f32; 4]| {
            let mut bytes = vec![0u8; row * size];
            for (i, t) in texels.iter().enumerate() {
                let at = (i / 2) * row + (i % 2) * 4;
                bytes[at..at + 4].copy_from_slice(&t.to_le_bytes());
            }
            bytes
        };
        let kept = [layer([0.5, 0.25, 0.0, 1.0]), layer([0.5; 4])].concat();
        let same = kept.clone();
        assert_eq!(
            compare_depth_layers(&kept, &same, size, row),
            LayerComparison::default()
        );
        let fresh = [layer([0.5, 0.25, 0.0, 1.0]), layer([0.5, 0.5, 0.75, 0.5])].concat();
        let result = compare_depth_layers(&kept, &fresh, size, row);
        assert_eq!(result.layers_differing, 1);
        assert_eq!(result.texels_differing, 1);
        assert_eq!(result.max_depth_delta, 0.25);
        // Padding bytes are not texels.
        let mut padded = kept.clone();
        padded[size * 4] = 0xff;
        assert_eq!(
            compare_depth_layers(&kept, &padded, size, row),
            LayerComparison::default()
        );
    }
}
