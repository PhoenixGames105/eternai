# Shim for the Vulkan SDK's SPIRV-Headers package.
#
# The SDK ships its config at Lib\cmake\SPIRV-Headers\SPIRV-HeadersConfig.cmake,
# but that file is generated for an install layout where the headers sit at
# Lib\cmake\SPIRV-Headers\include — in the SDK they live at Include\spirv, so
# the stock config fails its import check. This shim defines the same
# INTERFACE target against the real header location.
#
# Point CMake at this directory with -DSPIRV-Headers_DIR=<this dir> and pass
# -DVULKAN_SDK_ROOT=<SDK root> (build-android.ps1 does both).
if(NOT DEFINED VULKAN_SDK_ROOT)
    message(FATAL_ERROR "SPIRV-Headers shim: VULKAN_SDK_ROOT is not set")
endif()
set(_spirv_headers_include "${VULKAN_SDK_ROOT}/Include/spirv")
if(NOT IS_DIRECTORY "${_spirv_headers_include}")
    message(FATAL_ERROR "SPIRV-Headers shim: headers not found at ${_spirv_headers_include}")
endif()
if(NOT TARGET SPIRV-Headers::SPIRV-Headers)
    add_library(SPIRV-Headers::SPIRV-Headers INTERFACE IMPORTED)
    set_target_properties(SPIRV-Headers::SPIRV-Headers PROPERTIES
        INTERFACE_INCLUDE_DIRECTORIES "${_spirv_headers_include}")
endif()
