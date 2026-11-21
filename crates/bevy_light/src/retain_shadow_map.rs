use bevy_ecs::prelude::*;
use bevy_reflect::prelude::*;

/// Add this component to a [`PointLight`](crate::PointLight) or
/// [`SpotLight`](crate::SpotLight) that has shadow maps enabled to keep its
/// shadow map between frames instead of rendering it again each frame.
///
/// While the component is present and the light's shadow map has been rendered
/// once, the renderer skips the light's shadow views entirely (no shadow render
/// pass, no culling or queueing of shadow casters) and keeps the contents of
/// the light's shadow map for lighting to sample. The first frame, and any
/// frame without this component, render the shadow map as usual. The shadow map
/// is also rendered again whenever the light itself changes (its pose, range,
/// angles or near plane), or the renderer had to move or reallocate its shadow
/// maps.
///
/// The renderer **cannot see changes to shadow casters**. A mesh that moves,
/// appears or disappears inside the light's range does not change a retained
/// shadow map. Remove this component for a frame (or for as long as needed) to
/// have the shadow map rendered again.
///
/// Directional lights and their cascades ignore this component.
#[derive(Clone, Copy, Component, Default, Debug, Reflect)]
#[reflect(Component, Default, Debug, Clone)]
pub struct RetainShadowMap;
