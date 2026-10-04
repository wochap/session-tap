package dev.sessiontap.android.ui.components

import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.FlowRow
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.Icon
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.vector.ImageVector
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import com.adamglin.PhosphorIcons
import com.adamglin.phosphoricons.Regular
import com.adamglin.phosphoricons.regular.Eraser
import com.adamglin.phosphoricons.regular.Eye
import com.adamglin.phosphoricons.regular.Keyboard
import com.adamglin.phosphoricons.regular.ListBullets
import com.adamglin.phosphoricons.regular.Question
import dev.sessiontap.android.domain.ScopeChip
import dev.sessiontap.android.domain.Scopes
import dev.sessiontap.android.ui.theme.Mono
import dev.sessiontap.android.ui.theme.St

private fun scopeIcon(name: String): ImageVector = when (name) {
    Scopes.READ -> PhosphorIcons.Regular.ListBullets
    Scopes.MANAGE -> PhosphorIcons.Regular.Eraser
    Scopes.WATCH -> PhosphorIcons.Regular.Eye
    Scopes.CONTROL -> PhosphorIcons.Regular.Keyboard
    else -> PhosphorIcons.Regular.Question
}

/**
 * Scope chips; control uses the amber warning style. [compact] shows raw
 * names in small chips, as on hub cards. Each chip is tagged `[tagPrefix]<name>`.
 */
@Composable
fun ScopeChips(names: List<String>, tagPrefix: String, modifier: Modifier = Modifier, compact: Boolean = false) {
    FlowRow(modifier, horizontalArrangement = Arrangement.spacedBy(6.dp), verticalArrangement = Arrangement.spacedBy(6.dp)) {
        Scopes.chips(names, short = compact).forEach { ScopeChipView(it, compact, Modifier.testTag("$tagPrefix${it.name}")) }
    }
}

@Composable
private fun ScopeChipView(chip: ScopeChip, compact: Boolean, modifier: Modifier) {
    val c = St.colors
    val shape = RoundedCornerShape(if (compact) 6.dp else 8.dp)
    val ring = if (chip.warning) c.run.copy(alpha = 0.55f) else c.line
    val fill = if (chip.warning) c.run.copy(alpha = 0.14f) else Color.Transparent
    Row(
        modifier.height(if (compact) 22.dp else 30.dp).background(fill, shape).border(1.dp, ring, shape).padding(horizontal = if (compact) 7.dp else 10.dp),
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.spacedBy(if (compact) 4.dp else 6.dp),
    ) {
        Icon(scopeIcon(chip.name), null, tint = if (chip.warning) c.run else c.mute, modifier = Modifier.size(if (compact) 12.dp else 14.dp))
        Text(
            chip.label,
            fontSize = if (compact) 11.sp else 12.5.sp,
            fontWeight = FontWeight.Medium,
            fontFamily = if (compact) Mono else null,
            color = c.text,
        )
    }
}
