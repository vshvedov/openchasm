# Addon boss compatibility

The bundled modding manual describes an addon-specific condition: rocket type 20 must be reflected and hit authored monster ID 18, which unlocks and activates process 30 and kills the monster. Original addon data identifies monster 18 as the Wizard; the remaster's merged definition table places it at index 23. The authored placement ID remains 118, while actor stats and assets resolve through the addon mapping.

`Session::set_addon_boss_rule(ActorMapping::AddonToRemaster)` enables this behavior explicitly. It is disabled by default. The caller must choose the content context; `ActorMapping::recognize_shipped_addon` recognizes known unmodified map and definition bytes. The rule requires a projectile actually redirected by the player reflector; ordinary player shots and other actor or rocket IDs do not qualify.

Process 30 must exist, be a supported mission-only process and have an ending delay in the session's mission plan. The process unlock/activation and actor death commit together. If validation fails, the impact is not emitted and the projectile is restored for a later retry. The installed level 19 script meets these conditions; broader action-bearing boss procedures remain unsupported. Projectile movement, collision shape and reflector direction still use the existing provisional approximations.
