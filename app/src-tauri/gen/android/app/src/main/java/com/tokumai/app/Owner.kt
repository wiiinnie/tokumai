package com.tokumai.app

import android.app.Activity
import androidx.biometric.BiometricManager
import androidx.biometric.BiometricManager.Authenticators.BIOMETRIC_WEAK
import androidx.biometric.BiometricManager.Authenticators.DEVICE_CREDENTIAL
import androidx.biometric.BiometricPrompt
import androidx.core.content.ContextCompat
import androidx.fragment.app.FragmentActivity
import java.util.concurrent.atomic.AtomicInteger

/**
 * The owner check before the recovery phrase is shown (owner.rs): the system's prompt for
 * a fingerprint, a face or the device credential. Rust starts it and polls `poll()`:
 * 0 while the prompt is up, 1 verified, 2 not. A phone with no lock screen has nothing to
 * ask for and is let through — whoever holds it holds everything on it already.
 */
object Owner {
    private val state = AtomicInteger(0)

    @JvmStatic
    fun start(activity: Activity, reason: String) {
        state.set(0)
        val fa = activity as? FragmentActivity ?: run { state.set(2); return }
        val authenticators = BIOMETRIC_WEAK or DEVICE_CREDENTIAL
        when (BiometricManager.from(fa).canAuthenticate(authenticators)) {
            BiometricManager.BIOMETRIC_SUCCESS -> {}
            BiometricManager.BIOMETRIC_ERROR_NONE_ENROLLED -> { state.set(1); return }
            else -> { state.set(2); return }
        }
        fa.runOnUiThread {
            val callback = object : BiometricPrompt.AuthenticationCallback() {
                override fun onAuthenticationSucceeded(result: BiometricPrompt.AuthenticationResult) { state.set(1) }
                override fun onAuthenticationError(errorCode: Int, errString: CharSequence) { state.set(2) }
                // onAuthenticationFailed: a finger the phone did not know; the prompt stays up.
            }
            val info = BiometricPrompt.PromptInfo.Builder()
                .setTitle("tokumai")
                .setSubtitle(reason)
                .setAllowedAuthenticators(authenticators)
                .build()
            try {
                BiometricPrompt(fa, ContextCompat.getMainExecutor(fa), callback).authenticate(info)
            } catch (e: Exception) {
                state.set(2)
            }
        }
    }

    @JvmStatic
    fun poll(): Int = state.get()
}
