package com.tokumai.app

import android.Manifest
import android.app.Activity
import android.content.Intent
import android.content.pm.PackageManager
import android.os.Build
import android.os.Bundle
import android.speech.RecognitionListener
import android.speech.RecognizerIntent
import android.speech.SpeechRecognizer
import androidx.core.app.ActivityCompat
import androidx.core.content.ContextCompat
import org.json.JSONObject
import java.lang.ref.WeakReference
import java.util.Locale

/**
 * Speech to text that never leaves the phone (voice.rs). Android 12's on-device
 * recogniser (`createOnDeviceSpeechRecognizer`) transcribes what is said; only the text
 * goes on, as if typed. A phone without it is told so, and tokumai does not listen rather
 * than send the voice to Google. Nothing is recorded: the recogniser hears the microphone
 * directly and hands back words.
 *
 * Rust starts a session and polls `poll()` a dozen times a second for the state, the text
 * so far and the input level (the waveform). States: idle, permission (the system's
 * microphone prompt is up), listening, stopping, final, error.
 */
object Voice {
    private const val PERMISSION_REQUEST = 7331

    private val lock = Any()
    private var state = "idle"
    private var text = ""
    private var error = ""
    private var level = 0f
    private var lang = ""
    private var recognizer: SpeechRecognizer? = null
    private var activityRef: WeakReference<Activity>? = null

    private fun json(vararg pairs: Pair<String, Any?>): ByteArray {
        val o = JSONObject()
        for ((k, v) in pairs) if (v != null) o.put(k, v)
        return o.toString().toByteArray(Charsets.UTF_8)
    }

    private fun onDevice(activity: Activity): String? {
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.S) return "on-device speech recognition needs Android 12"
        if (!SpeechRecognizer.isOnDeviceRecognitionAvailable(activity)) {
            return "this phone has no on-device speech recognition — your voice would have to leave it, so tokumai does not listen"
        }
        return null
    }

    /** Said before the microphone button is shown. */
    @JvmStatic
    fun available(activity: Activity): ByteArray {
        val why = onDevice(activity)
        return if (why == null) json("available" to true, "lang" to Locale.getDefault().toLanguageTag())
        else json("available" to false, "why" to why)
    }

    @JvmStatic
    fun start(activity: Activity, language: String): ByteArray {
        synchronized(lock) {
            if (state == "listening" || state == "stopping" || state == "permission") return json("error" to "already listening")
            state = "idle"; text = ""; error = ""; level = 0f; lang = language
        }
        onDevice(activity)?.let { return json("error" to it) }
        activityRef = WeakReference(activity)
        if (ContextCompat.checkSelfPermission(activity, Manifest.permission.RECORD_AUDIO) != PackageManager.PERMISSION_GRANTED) {
            synchronized(lock) { state = "permission" }
            ActivityCompat.requestPermissions(activity, arrayOf(Manifest.permission.RECORD_AUDIO), PERMISSION_REQUEST)
            return json("ok" to true, "permission" to "asked")
        }
        listen(activity)
        return json("ok" to true)
    }

    /** MainActivity hands the answer to the microphone prompt here. */
    @JvmStatic
    fun onPermission(requestCode: Int, granted: Boolean) {
        if (requestCode != PERMISSION_REQUEST) return
        val waiting = synchronized(lock) { state == "permission" }
        if (!waiting) return
        val activity = activityRef?.get()
        if (granted && activity != null) {
            listen(activity)
        } else {
            synchronized(lock) {
                state = "error"
                error = "the microphone is not allowed for tokumai — Settings → Apps → tokumai → Permissions"
            }
        }
    }

    private fun listen(activity: Activity) {
        activity.runOnUiThread {
            try {
                if (Build.VERSION.SDK_INT < Build.VERSION_CODES.S) throw IllegalStateException("needs Android 12")
                // The promise: the on-device recogniser, never the default one that may
                // send the audio away.
                val r = SpeechRecognizer.createOnDeviceSpeechRecognizer(activity)
                r.setRecognitionListener(object : RecognitionListener {
                    override fun onReadyForSpeech(params: Bundle?) {}
                    override fun onBeginningOfSpeech() {}
                    // rmsdB runs roughly −2 (silence) to 10 (loud); mapped to 0…1.
                    override fun onRmsChanged(rmsdB: Float) {
                        synchronized(lock) { level = ((rmsdB + 2f) / 12f).coerceIn(0f, 1f) }
                    }
                    override fun onBufferReceived(buffer: ByteArray?) {}
                    override fun onEndOfSpeech() {}
                    override fun onError(code: Int) {
                        synchronized(lock) {
                            if (state == "final" || state == "idle") return
                            // Nothing heard is an empty answer, not a fault; a fault
                            // while listening is said.
                            val nothing = code == SpeechRecognizer.ERROR_NO_MATCH || code == SpeechRecognizer.ERROR_SPEECH_TIMEOUT
                            if (state == "stopping" || nothing) {
                                state = "final"
                            } else {
                                state = "error"
                                error = describe(code)
                            }
                        }
                        release()
                    }
                    override fun onResults(results: Bundle?) {
                        val m = results?.getStringArrayList(SpeechRecognizer.RESULTS_RECOGNITION)
                        synchronized(lock) {
                            if (!m.isNullOrEmpty() && m[0].isNotEmpty()) text = m[0]
                            state = "final"
                        }
                        release()
                    }
                    override fun onPartialResults(partialResults: Bundle?) {
                        val m = partialResults?.getStringArrayList(SpeechRecognizer.RESULTS_RECOGNITION)
                        if (!m.isNullOrEmpty() && m[0].isNotEmpty()) synchronized(lock) { text = m[0] }
                    }
                    override fun onEvent(eventType: Int, params: Bundle?) {}
                })
                val intent = Intent(RecognizerIntent.ACTION_RECOGNIZE_SPEECH).apply {
                    putExtra(RecognizerIntent.EXTRA_LANGUAGE_MODEL, RecognizerIntent.LANGUAGE_MODEL_FREE_FORM)
                    putExtra(RecognizerIntent.EXTRA_PARTIAL_RESULTS, true)
                    putExtra(RecognizerIntent.EXTRA_PREFER_OFFLINE, true)
                    putExtra(RecognizerIntent.EXTRA_CALLING_PACKAGE, activity.packageName)
                    if (lang.isNotEmpty()) putExtra(RecognizerIntent.EXTRA_LANGUAGE, lang)
                }
                synchronized(lock) { recognizer = r; state = "listening" }
                r.startListening(intent)
            } catch (e: Exception) {
                synchronized(lock) {
                    state = "error"
                    error = "the microphone could not be started: ${e.message}"
                }
            }
        }
    }

    private fun describe(code: Int): String = when (code) {
        SpeechRecognizer.ERROR_INSUFFICIENT_PERMISSIONS -> "the microphone is not allowed for tokumai — Settings → Apps → tokumai → Permissions"
        SpeechRecognizer.ERROR_AUDIO -> "the microphone could not be opened"
        SpeechRecognizer.ERROR_RECOGNIZER_BUSY -> "the speech recogniser is busy — try again in a moment"
        SpeechRecognizer.ERROR_LANGUAGE_NOT_SUPPORTED, SpeechRecognizer.ERROR_LANGUAGE_UNAVAILABLE ->
            "no on-device speech model for your language — Settings → System → Languages → On-device speech recognition"
        else -> "speech recognition failed (error $code)"
    }

    @JvmStatic
    fun poll(): ByteArray = synchronized(lock) {
        json("state" to state, "text" to text, "level" to level.toDouble(), "error" to error.ifEmpty { null })
    }

    /** The person is done talking; the final words come through the listener. */
    @JvmStatic
    fun stop() {
        val r: SpeechRecognizer?
        synchronized(lock) {
            if (state != "listening") return
            state = "stopping"
            r = recognizer
        }
        val activity = activityRef?.get()
        if (activity != null) activity.runOnUiThread { r?.stopListening() } else r?.stopListening()
    }

    @JvmStatic
    fun cancel() {
        val r: SpeechRecognizer?
        synchronized(lock) {
            state = "idle"; text = ""; error = ""; level = 0f
            r = recognizer; recognizer = null
        }
        activityRef?.get()?.runOnUiThread { r?.cancel(); r?.destroy() }
    }

    private fun release() {
        val r: SpeechRecognizer?
        synchronized(lock) { r = recognizer; recognizer = null; level = 0f }
        activityRef?.get()?.runOnUiThread { r?.destroy() }
    }
}
