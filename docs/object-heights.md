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
first teleport landing. Until the host installs a trustworthy catalog and
threads it through all collision paths, preserve the previous conservative
behavior and do not mark unknown meshes as clear.
