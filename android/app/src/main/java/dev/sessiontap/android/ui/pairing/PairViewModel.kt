package dev.sessiontap.android.ui.pairing

import android.app.Application
import android.os.Build
import androidx.lifecycle.AndroidViewModel
import androidx.lifecycle.viewModelScope
import dev.sessiontap.android.SessionTapApp
import dev.sessiontap.android.crypto.DeviceKey
import dev.sessiontap.android.domain.QrResult
import dev.sessiontap.android.domain.ValidPayload
import dev.sessiontap.android.domain.validateQr
import dev.sessiontap.android.net.EndpointFailure
import dev.sessiontap.android.net.HubTls
import dev.sessiontap.android.net.PairOutcome
import dev.sessiontap.android.net.pairWithHub
import dev.sessiontap.android.service.HubService
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.launch

sealed interface PairState {
    data object Idle : PairState
    data class Connecting(val hubName: String) : PairState
    data class Waiting(
        val hubName: String,
        val deviceName: String,
        val fingerprint: List<String>,
        val expiresAt: Long,
        /** Scopes the QR code requests. */
        val scopes: List<String> = emptyList(),
    ) : PairState
    data class Paired(val hubName: String, val endpoint: String, val hubId: String) : PairState
    data object Expired : PairState
    data class Rejected(val hubName: String) : PairState
    data class Failed(val hubName: String, val message: String) : PairState
    data class Unreachable(val hubName: String, val failures: List<EndpointFailure>) : PairState
    data class Invalid(val message: String) : PairState
}

/** Drives one pairing attempt from a scanned (or debug deep-linked) QR payload. */
class PairViewModel(application: Application) : AndroidViewModel(application) {
    private val app = application as SessionTapApp
    private val _state = MutableStateFlow<PairState>(PairState.Idle)
    val state: StateFlow<PairState> = _state
    private var job: Job? = null
    private var last: ValidPayload? = null

    val deviceName: String = "${Build.MANUFACTURER.replaceFirstChar { it.uppercase() }} ${Build.MODEL}".trim()

    /** Returns false when the text is not a usable payload (the state says why). */
    fun start(qrText: String): Boolean {
        return when (val result = validateQr(qrText, System.currentTimeMillis() / 1000)) {
            is QrResult.Valid -> {
                run(result.value)
                true
            }
            QrResult.Expired -> {
                _state.value = PairState.Expired
                true
            }
            is QrResult.Invalid -> {
                _state.value = PairState.Invalid(result.why)
                false
            }
        }
    }

    fun retry() {
        last?.let { run(it) }
    }

    fun cancel() {
        job?.cancel()
        job = null
        _state.value = PairState.Idle
    }

    private fun run(valid: ValidPayload) {
        last = valid
        job?.cancel()
        val hubName = valid.payload.hub
        _state.value = PairState.Connecting(hubName)
        job = viewModelScope.launch(Dispatchers.IO) {
            val spki = DeviceKey.spki()
            val fingerprint = DeviceKey.fingerprint()
            val outcome = pairWithHub(
                valid = valid,
                deviceName = deviceName,
                deviceSpki = spki,
                clientFor = { HubTls.client(valid.payload.id, DeviceKey.keyManager()) },
                onWaiting = { _state.value = PairState.Waiting(hubName, deviceName, fingerprint, valid.payload.exp * 1000, valid.payload.sc) },
            )
            _state.value = when (outcome) {
                is PairOutcome.Paired -> {
                    app.repository.savePaired(outcome.hubId, outcome.hubName, valid.payload.ep, outcome.scopes, outcome.endpoint)
                    HubService.start(app)
                    PairState.Paired(outcome.hubName, outcome.endpoint, outcome.hubId)
                }
                PairOutcome.Expired -> PairState.Expired
                PairOutcome.Rejected -> PairState.Rejected(hubName)
                is PairOutcome.Failed -> PairState.Failed(hubName, outcome.message)
                is PairOutcome.Unreachable -> PairState.Unreachable(hubName, outcome.failures)
            }
        }
    }
}
