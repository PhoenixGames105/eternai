package dev.eternai.app.ui

import androidx.compose.foundation.isSystemInDarkTheme
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.darkColorScheme
import androidx.compose.material3.lightColorScheme
import androidx.compose.runtime.Composable
import androidx.compose.ui.graphics.Color

private val Accent = Color(0xFF7BD1C4)
private val AccentDim = Color(0xFF3E6E68)
private val SurfaceDark = Color(0xFF0B0F14)
private val SurfaceDarkAlt = Color(0xFF141A22)
private val SurfaceLight = Color(0xFFF6F8F8)

private val DarkScheme = darkColorScheme(
    primary = Accent,
    onPrimary = Color(0xFF06201C),
    secondary = AccentDim,
    surface = SurfaceDark,
    surfaceVariant = SurfaceDarkAlt,
    onSurface = Color(0xFFE6EDF0),
    onSurfaceVariant = Color(0xFF9AA7B0),
    error = Color(0xFFFF8A80),
)

private val LightScheme = lightColorScheme(
    primary = Color(0xFF146B60),
    surface = SurfaceLight,
    surfaceVariant = Color(0xFFE7EDEB),
)

@Composable
fun EternAITheme(
    darkTheme: Boolean = isSystemInDarkTheme(),
    content: @Composable () -> Unit,
) {
    MaterialTheme(
        colorScheme = if (darkTheme) DarkScheme else LightScheme,
        content = content,
    )
}
