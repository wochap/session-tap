package dev.sessiontap.android.ui.theme

import androidx.compose.foundation.isSystemInDarkTheme
import androidx.compose.material3.ColorScheme
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Typography
import androidx.compose.material3.darkColorScheme
import androidx.compose.material3.lightColorScheme
import androidx.compose.runtime.Composable
import androidx.compose.runtime.CompositionLocalProvider
import androidx.compose.runtime.Immutable
import androidx.compose.runtime.staticCompositionLocalOf
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.ExperimentalTextApi
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.font.Font
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontVariation
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.em
import androidx.compose.ui.unit.sp
import dev.sessiontap.android.R
import kotlin.math.cos
import kotlin.math.pow
import kotlin.math.roundToInt
import kotlin.math.sin

/** Nocturne tokens plus the OKLCH status hues from the handoff (converted to sRGB once). */
@Immutable
data class SessionTapColors(
    val bg: Color,
    val surf: Color,
    val surf2: Color,
    val line: Color,
    val text: Color,
    val mute: Color,
    val dim: Color,
    val acc: Color,
    val accInk: Color,
    val accTint: Color,
    val ind: Color,
    val run: Color,
    val block: Color,
    val ok: Color,
    val blockTint: Color,
    val inv: Color,
    val invText: Color,
    val invAcc: Color,
    val dark: Boolean,
)

val DarkColors = SessionTapColors(
    bg = Color(0xFF0A0B14),
    surf = Color(0xFF0C0D16),
    surf2 = Color(0xFF232532),
    line = Color(0x17E9E9ED),
    text = Color(0xFFE9E9ED),
    mute = Color(0xFF9397AB),
    dim = Color(0xFF595D6C),
    acc = Color(0xFF9184D9),
    accInk = Color(0xFFD2CEFD),
    accTint = Color(0x249184D9),
    ind = Color(0xFF2B2741),
    run = Color(0xFFF2AF48),
    block = Color(0xFFFB605C),
    ok = Color(0xFF5CCB89),
    blockTint = Color(0x1AFB605C),
    inv = Color(0xFFE4E7F5),
    invText = Color(0xFF292B31),
    invAcc = Color(0xFF5D5294),
    dark = true,
)

val LightColors = SessionTapColors(
    bg = Color(0xFFF3F5FE),
    surf = Color(0xFFEBEDF9),
    surf2 = Color(0xFFE4E7F5),
    line = Color(0x1C292B31),
    text = Color(0xFF292B31),
    mute = Color(0xFF75798C),
    dim = Color(0xFFB2B6CA),
    acc = Color(0xFF796CBF),
    accInk = Color(0xFF5D5294),
    accTint = Color(0x24968AE0),
    ind = Color(0xFFE7E5FE),
    run = Color(0xFFC97500),
    block = Color(0xFFD02B31),
    ok = Color(0xFF008B4E),
    blockTint = Color(0x14D02B31),
    inv = Color(0xFF3F424D),
    invText = Color(0xFFF3F5FE),
    invAcc = Color(0xFFD2CEFD),
    dark = false,
)

val LocalColors = staticCompositionLocalOf { DarkColors }

@OptIn(ExperimentalTextApi::class)
private fun inter(weight: Int) = Font(
    R.font.inter,
    weight = FontWeight(weight),
    variationSettings = FontVariation.Settings(FontVariation.weight(weight)),
)

@OptIn(ExperimentalTextApi::class)
private fun mono(weight: Int) = Font(
    R.font.jetbrains_mono,
    weight = FontWeight(weight),
    variationSettings = FontVariation.Settings(FontVariation.weight(weight)),
)

val Inter = FontFamily(inter(400), inter(500), inter(600), inter(700))
val Mono = FontFamily(mono(400), mono(500), mono(600))

private fun typography(): Typography {
    val base = TextStyle(fontFamily = Inter, fontSize = 14.sp, lineHeight = 1.4.em)
    return Typography(
        headlineLarge = base.copy(fontSize = 34.sp, fontWeight = FontWeight.Medium, lineHeight = 1.1.em, letterSpacing = (-0.02).em),
        headlineMedium = base.copy(fontSize = 28.sp, fontWeight = FontWeight.Medium, lineHeight = 1.15.em, letterSpacing = (-0.02).em),
        headlineSmall = base.copy(fontSize = 24.sp, fontWeight = FontWeight.Medium, lineHeight = 1.2.em, letterSpacing = (-0.02).em),
        titleLarge = base.copy(fontSize = 22.sp, fontWeight = FontWeight.Medium, letterSpacing = (-0.015).em),
        titleMedium = base.copy(fontSize = 16.sp, fontWeight = FontWeight.Medium),
        titleSmall = base.copy(fontSize = 14.sp, fontWeight = FontWeight.Medium),
        bodyLarge = base.copy(fontSize = 15.sp),
        bodyMedium = base,
        bodySmall = base.copy(fontSize = 12.sp),
        labelLarge = base.copy(fontSize = 14.sp, fontWeight = FontWeight.Medium),
        labelMedium = base.copy(fontSize = 12.sp, fontWeight = FontWeight.Medium),
        labelSmall = base.copy(fontSize = 11.sp),
    )
}

private fun scheme(c: SessionTapColors): ColorScheme {
    val s = if (c.dark) darkColorScheme() else lightColorScheme()
    return s.copy(
        primary = c.acc,
        onPrimary = c.bg,
        primaryContainer = c.ind,
        onPrimaryContainer = c.text,
        secondaryContainer = c.ind,
        onSecondaryContainer = c.text,
        background = c.bg,
        onBackground = c.text,
        surface = c.bg,
        onSurface = c.text,
        surfaceVariant = c.surf,
        onSurfaceVariant = c.mute,
        surfaceContainer = c.surf,
        surfaceContainerHigh = c.surf2,
        surfaceContainerHighest = c.surf2,
        surfaceContainerLow = c.surf,
        outline = c.dim,
        outlineVariant = c.line,
        error = c.block,
        inverseSurface = c.inv,
        inverseOnSurface = c.invText,
        inversePrimary = c.invAcc,
    )
}

@Composable
fun SessionTapTheme(dark: Boolean = isSystemInDarkTheme(), content: @Composable () -> Unit) {
    val colors = if (dark) DarkColors else LightColors
    CompositionLocalProvider(LocalColors provides colors) {
        MaterialTheme(colorScheme = scheme(colors), typography = typography(), content = content)
    }
}

object St {
    val colors: SessionTapColors
        @Composable get() = LocalColors.current
}

/** OKLCH to sRGB, used for per-repository hues. */
fun oklch(l: Double, c: Double, hDeg: Double): Color {
    val h = Math.toRadians(hDeg)
    val a = c * cos(h)
    val b = c * sin(h)
    val l_ = (l + 0.3963377774 * a + 0.2158037573 * b).pow(3)
    val m_ = (l - 0.1055613458 * a - 0.0638541728 * b).pow(3)
    val s_ = (l - 0.0894841775 * a - 1.2914855480 * b).pow(3)
    fun enc(x: Double): Int {
        val v = if (x <= 0.0031308) 12.92 * x else 1.055 * x.pow(1 / 2.4) - 0.055
        return (v.coerceIn(0.0, 1.0) * 255).roundToInt()
    }
    val r = enc(4.0767416621 * l_ - 3.3077115913 * m_ + 0.2309699292 * s_)
    val g = enc(-1.2684380046 * l_ + 2.6097574011 * m_ - 0.3413193965 * s_)
    val bl = enc(-0.0041960863 * l_ - 0.7034186147 * m_ + 1.7076147010 * s_)
    return Color(r, g, bl)
}

/** Stable hue per repository root, like the handoff's repo squares. */
fun repoColor(key: String?): Color {
    if (key.isNullOrEmpty()) return oklch(0.66, 0.02, 280.0)
    val hue = ((key.hashCode().toLong() and 0xffffffffL) % 360).toDouble()
    return oklch(0.72, 0.11, hue)
}
