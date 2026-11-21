use bevy_ecs::prelude::*;
use bevy_reflect::prelude::*;

/// Add this component to a [`PointLight`](crate::PointLight) or
/// [`SpotLight`](crate::SpotLight) that has shadow maps enabled to keep its
/// shadow map between frames instead of rendering it again each frame.
///
/// Once the light's shadow map has been rendered completely (every caster
/// queued, every pipeline ready) the renderer skips the light's shadow views
/// entirely (no shadow render pass, no culling or queueing of shadow casters)
/// and keeps the contents of the light's shadow map for lighting to sample.
/// Until then, and for any frame without this component, the shadow map is
/// rendered as usual.
///
/// A kept shadow map stays where it is in the shadow map arrays while its
/// light stays shadowed, whatever other lights do (see [`ShadowMapCapacity`]
/// for how big the arrays are). It is rendered again when the light itself
/// changes (its pose, range, angles, near plane or render layers), when the
/// arrays had to be reallocated, or when [`generation`](Self::generation)
/// changes.
///
/// The renderer **cannot see changes to shadow casters**. A mesh that moves,
/// appears or disappears inside the light's range does not change a retained
/// shadow map. Ask for a new render by bumping [`generation`](Self::generation)
/// (cheaper and steadier than removing and adding the component).
///
/// Directional lights and their cascades ignore this component.
#[derive(Clone, Copy, Component, Default, Debug, PartialEq, Eq, Reflect)]
#[reflect(Component, Default, Debug, Clone, PartialEq)]
pub struct RetainShadowMap {
    /// Part of what the kept shadow map was rendered with. The renderer renders
    /// the map again, once, whenever this differs from the value it was last
    /// rendered with. Bump it (with `wrapping_add(1)`) to ask for one more
    /// render, for example after a caster in range of the light moved.
    pub generation: u32,
}

impl RetainShadowMap {
    /// A marker with the given [`generation`](Self::generation).
    pub const fn new(generation: u32) -> Self {
        Self { generation }
    }
}

/// How many shadow maps the point and spot light shadow map arrays have room
/// for, no matter how many lights are shadowed right now.
///
/// The renderer allocates one slot per shadowed light and keeps a light in its
/// slot while it stays shadowed, so the lights that come and go don't move the
/// maps kept by [`RetainShadowMap`]. The arrays are as big as the highest slot
/// in use or this capacity, whichever is more. Growing the arrays (more lights
/// than slots) allocates new arrays, and the maps they held are rendered again;
/// set a capacity that covers the most lights you expect to avoid that. A
/// slot of a point light is a cube (six layers), a slot of a spot light one
/// layer.
///
/// While no light has a [`RetainShadowMap`], slots are handed out in the
/// lights' sorted order every frame and the arrays have exactly one slot per
/// shadowed light (and so today's behaviour, if the capacity is 0). While one
/// has, slots are kept and the arrays only shrink when no light has one.
#[derive(Resource, Clone, Copy, Debug, Default, PartialEq, Eq, Reflect)]
#[reflect(Resource, Default, Debug, Clone, PartialEq)]
pub struct ShadowMapCapacity {
    /// Point light shadow map slots (cubes) to allocate. `0` allocates only
    /// what shadowed point lights need.
    pub point_lights: usize,
    /// Spot light shadow map slots (layers) to allocate. `0` allocates only
    /// what shadowed spot lights need.
    pub spot_lights: usize,
}
