package dev.sessiontap.android.ui.pairing

import android.Manifest
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.contract.ActivityResultContracts
import androidx.camera.core.CameraSelector
import androidx.camera.core.ImageAnalysis
import androidx.camera.core.Preview
import androidx.camera.lifecycle.ProcessCameraProvider
import androidx.camera.view.PreviewView
import androidx.compose.foundation.Canvas
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.geometry.CornerRadius
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.Path
import androidx.compose.ui.graphics.drawscope.Stroke
import androidx.compose.ui.graphics.drawscope.rotate
import androidx.compose.ui.platform.LocalContext
import androidx.lifecycle.compose.LocalLifecycleOwner
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.compose.ui.viewinterop.AndroidView
import androidx.core.content.ContextCompat
import com.adamglin.PhosphorIcons
import com.adamglin.phosphoricons.Regular
import com.adamglin.phosphoricons.regular.X
import dev.sessiontap.android.ui.components.PrimaryButton
import dev.sessiontap.android.ui.onboarding.permissionState
import dev.sessiontap.android.ui.theme.Mono
import dev.sessiontap.android.ui.theme.St
import zxingcpp.BarcodeReader
import java.util.concurrent.Executors
import java.util.concurrent.atomic.AtomicBoolean

@Composable
fun ScanScreen(onClose: () -> Unit, onScanned: (String) -> Boolean, error: String?, contentPadding: PaddingValues) {
    val c = St.colors
    val context = LocalContext.current
    var granted by remember { mutableStateOf(permissionState(context).camera) }
    val ask = rememberLauncherForActivityResult(ActivityResultContracts.RequestPermission()) { granted = it }
    LaunchedEffect(Unit) { if (!granted) ask.launch(Manifest.permission.CAMERA) }
    val text = Color(0xFFE9E9ED)
    Box(Modifier.fillMaxSize().background(Color(0xFF000001))) {
        if (granted) CameraPreview(onScanned)
        Column(Modifier.fillMaxSize().padding(contentPadding)) {
            Row(Modifier.padding(horizontal = 12.dp, vertical = 8.dp), verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                IconButton(onClick = onClose) { Icon(PhosphorIcons.Regular.X, "Close", tint = text, modifier = Modifier.size(22.dp)) }
                Text("Scan pairing code", fontSize = 17.sp, fontWeight = FontWeight.Medium, color = text)
            }
            Box(Modifier.weight(1f).fillMaxWidth(), contentAlignment = Alignment.Center) {
                Viewfinder(c.acc)
                if (!granted) {
                    Column(horizontalAlignment = Alignment.CenterHorizontally, verticalArrangement = Arrangement.spacedBy(12.dp), modifier = Modifier.padding(32.dp)) {
                        Text("Camera access is needed to scan the code.", color = text, fontSize = 14.sp)
                        PrimaryButton("Allow camera", { ask.launch(Manifest.permission.CAMERA) }, height = 40.dp)
                    }
                }
            }
            Column(Modifier.padding(start = 32.dp, end = 32.dp, bottom = 40.dp), verticalArrangement = Arrangement.spacedBy(6.dp)) {
                Text(error ?: "Point at the QR in your terminal", fontSize = 15.sp, fontWeight = FontWeight.Medium, color = if (error != null) c.block else text)
                Row {
                    Text("From ", fontSize = 13.sp, color = Color(0xFFB2B6CA))
                    Text("sessiontap-hub pair", fontSize = 13.sp, color = Color(0xFFB2B6CA), fontFamily = Mono)
                    Text(". The code expires after 2 minutes.", fontSize = 13.sp, color = Color(0xFFB2B6CA))
                }
            }
        }
    }
}

@Composable
private fun Viewfinder(accent: Color) {
    Canvas(Modifier.size(248.dp)) {
        val r = 28.dp.toPx()
        val len = 36.dp.toPx()
        val w = 3.dp.toPx()
        val s = size.width
        // dim everything outside the window
        val outside = Path().apply {
            addRect(androidx.compose.ui.geometry.Rect(-2000f, -2000f, s + 2000f, s + 2000f))
            addRoundRect(androidx.compose.ui.geometry.RoundRect(0f, 0f, s, s, CornerRadius(r)))
            fillType = androidx.compose.ui.graphics.PathFillType.EvenOdd
        }
        drawPath(outside, Color(0x66161826))
        val corner = Path().apply {
            moveTo(0f, len)
            lineTo(0f, r)
            arcTo(androidx.compose.ui.geometry.Rect(0f, 0f, 2 * r, 2 * r), 180f, 90f, false)
            lineTo(len, 0f)
        }
        repeat(4) { i ->
            rotate(90f * i) { drawPath(corner, accent, style = Stroke(w)) }
        }
    }
}

@Composable
private fun CameraPreview(onScanned: (String) -> Boolean) {
    val context = LocalContext.current
    val owner = LocalLifecycleOwner.current
    val executor = remember { Executors.newSingleThreadExecutor() }
    val done = remember { AtomicBoolean(false) }
    val reader = remember {
        BarcodeReader().apply { options.formats = setOf(BarcodeReader.Format.QR_CODE) }
    }
    DisposableEffect(Unit) { onDispose { executor.shutdown() } }
    AndroidView(
        modifier = Modifier.fillMaxSize(),
        factory = { ctx ->
            val view = PreviewView(ctx).apply { scaleType = PreviewView.ScaleType.FILL_CENTER }
            val future = ProcessCameraProvider.getInstance(ctx)
            future.addListener({
                val provider = future.get()
                val preview = Preview.Builder().build().also { it.surfaceProvider = view.surfaceProvider }
                val analysis = ImageAnalysis.Builder()
                    .setBackpressureStrategy(ImageAnalysis.STRATEGY_KEEP_ONLY_LATEST)
                    .build()
                analysis.setAnalyzer(executor) { image ->
                    image.use {
                        if (done.get()) return@use
                        val text = runCatching { reader.read(it) }.getOrNull()?.firstOrNull()?.text ?: return@use
                        ContextCompat.getMainExecutor(context).execute {
                            if (!done.get() && onScanned(text)) done.set(true)
                        }
                    }
                }
                provider.unbindAll()
                provider.bindToLifecycle(owner, CameraSelector.DEFAULT_BACK_CAMERA, preview, analysis)
            }, ContextCompat.getMainExecutor(ctx))
            view
        },
    )
}
