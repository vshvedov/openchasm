# SDL audio is not used by OpenChasm; rodio provides audio. SDL2 2.26's
# PipeWire driver is incompatible with newer PipeWire headers and GCC 16.
set(SDL_PIPEWIRE OFF CACHE BOOL "Use SDL PipeWire audio" FORCE)
# sdl2-sys normally supplies these flags itself, but skips them when a custom
# SDL2_TOOLCHAIN is present. Preserve compatibility with newer C compilers.
set(CMAKE_C_STANDARD 99 CACHE STRING "C language standard" FORCE)
# Match sdl2-sys's static-build default and avoid a duplicate MSVC _fltused.
add_compile_definitions(__FLTUSED__)
if(CMAKE_HOST_SYSTEM_NAME STREQUAL "Linux" AND NOT CMAKE_SYSTEM_NAME STREQUAL "Windows")
  add_compile_options(-fcommon)
endif()
