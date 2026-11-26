# Host toolchain for the vulkan-shaders-gen ExternalProject.
#
# That sub-build compiles a small Windows host tool (vulkan-shaders-gen) that
# runs glslc over the shader sources. It must be built with a real host
# compiler, not the NDK clang (which targets Android and cannot link a Windows
# .exe). The sub-configure inherits the NDK compiler from the parent
# environment, so we override it here with MSVC.
#
# CMAKE_SYSTEM_NAME is deliberately NOT set: the parent build is cross
# (CMAKE_CROSSCOMPILING=TRUE), and ggml-vulkan only forwards this toolchain to
# the sub-configure in that case. Setting it to Windows would flip
# CMAKE_CROSSCOMPILING to FALSE and the toolchain would be ignored.
set(CMAKE_C_COMPILER cl)
set(CMAKE_CXX_COMPILER cl)
set(CMAKE_RC_COMPILER rc)
