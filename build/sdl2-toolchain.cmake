# A custom toolchain suppresses the cmake crate's normal target inference.
# Restore it for cross-OS builds so CMake cannot silently compile host SDL
# objects for a different Cargo target. The compiler and SDK are supplied by
# Cargo's normal target-specific CC/CXX environment or the user.
if(DEFINED ENV{CARGO_CFG_TARGET_OS})
  if("$ENV{CARGO_CFG_TARGET_OS}" STREQUAL "windows")
    set(_openchasm_target_system "Windows")
  elseif("$ENV{CARGO_CFG_TARGET_OS}" STREQUAL "macos")
    set(_openchasm_target_system "Darwin")
  elseif("$ENV{CARGO_CFG_TARGET_OS}" STREQUAL "linux")
    set(_openchasm_target_system "Linux")
  else()
    message(FATAL_ERROR "Unsupported SDL target OS: $ENV{CARGO_CFG_TARGET_OS}")
  endif()
  if(NOT _openchasm_target_system STREQUAL CMAKE_HOST_SYSTEM_NAME)
    if(DEFINED CMAKE_SYSTEM_NAME AND NOT CMAKE_SYSTEM_NAME STREQUAL _openchasm_target_system)
      message(FATAL_ERROR "SDL target OS conflicts with CMAKE_SYSTEM_NAME; configure a toolchain for $ENV{TARGET}")
    endif()
    set(CMAKE_SYSTEM_NAME "${_openchasm_target_system}" CACHE STRING "SDL target system")
    if(DEFINED ENV{CARGO_CFG_TARGET_ARCH} AND NOT DEFINED CMAKE_SYSTEM_PROCESSOR)
      if("$ENV{CARGO_CFG_TARGET_ARCH}" STREQUAL "aarch64" AND _openchasm_target_system STREQUAL "Darwin")
        set(CMAKE_SYSTEM_PROCESSOR "arm64" CACHE STRING "SDL target processor")
      elseif("$ENV{CARGO_CFG_TARGET_ARCH}" STREQUAL "aarch64" AND _openchasm_target_system STREQUAL "Windows")
        set(CMAKE_SYSTEM_PROCESSOR "ARM64" CACHE STRING "SDL target processor")
      elseif("$ENV{CARGO_CFG_TARGET_ARCH}" STREQUAL "x86_64" AND _openchasm_target_system STREQUAL "Windows")
        set(CMAKE_SYSTEM_PROCESSOR "AMD64" CACHE STRING "SDL target processor")
      else()
        set(CMAKE_SYSTEM_PROCESSOR "$ENV{CARGO_CFG_TARGET_ARCH}" CACHE STRING "SDL target processor")
      endif()
    endif()
    message(STATUS "OpenChasm SDL cross target: ${CMAKE_SYSTEM_NAME}; use a matching C compiler and SDK")
  endif()
endif()

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
