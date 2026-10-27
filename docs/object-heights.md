# Object collision heights

`object_heights` provides validated local vertical bounds and a catalog keyed by
raw map type. `HeightBounds::from_3o` uses base mesh vertex Z divided by 2048;
`HeightBounds::from_gltf_base` consumes basis-frame positions and the same
column-major accumulated node transforms used by the renderer. Empty or invalid
geometry produces no bounds. `ObjectHeightCatalog::overlaps` compares a mover's
vertical interval with a model's bounds plus its process-owned elevation. A
missing type, invalid input or invalid height always returns overlapping;
touching bounds are also considered overlap. The host must load and validate
meshes and provide the correct current raw type after scripted replacement.

The independent PanzerChasm reconstruction derives collision Z bounds from
base 3O vertices, even for animated models, and filters collision when those
bounds and the entity's height do not overlap. Direct comparison to the
original and remaster remains outstanding. glTF node transforms follow the
existing renderer, but morph animation, skeletal animation and dynamic shape
changes are not considered. Do not substitute an animated visual pose for the
base reference bounds without evidence.

In installed level 17, `18_cl2.3o` is raw type 165. Its unanimated vertex Z
range is 4096–9626, or 2.0–4.700195 tiles. The player is below this range at
flat floor height, so a horizontal-radius-only check incorrectly rejects the
first teleport landing. The client loads a catalog for every level-object definition and supplies it to the session. An installed-content regression exercises this teleport with the roof retained: the conservative path blocks the landing, while the loaded base bounds permit it. Unknown or invalid assets remain conservatively blocking.

The client loader caches bounds by model path, follows the renderer's explicit 3O versus glTF-only selection, and rejects empty or invalid geometry. It derives bounds from the base pose, including each glTF node transform; replacement definitions are loaded as well so later type changes use their own bounds. If the session rejects the catalog's cell mapping, the preview retains conservative collision and reports the failure.
