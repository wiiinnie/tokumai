package com.tokumai.app

import android.security.keystore.KeyGenParameterSpec
import android.security.keystore.KeyProperties
import java.security.KeyStore
import javax.crypto.Cipher
import javax.crypto.KeyGenerator
import javax.crypto.SecretKey
import javax.crypto.spec.GCMParameterSpec

/**
 * The at-rest key's keeper (keystore.rs, `android::wrapped_key`): an AES-256 key in the
 * Android Keystore — hardware-backed where the phone has it, never exportable — wraps the
 * 32 bytes the app encrypts its files with. What lies on disk is the wrapped form; a copy
 * of the app's storage is worthless without this phone's keystore.
 *
 * Called from Rust over JNI: `wrap(bytes)` → iv (12) ‖ ciphertext ‖ tag, `unwrap` the
 * reverse. Any failure is an exception, which Rust reports as an error and nothing is
 * written.
 */
object Keystore {
    private const val ALIAS = "tokumai-at-rest-wrap"
    private const val STORE = "AndroidKeyStore"
    private const val IV_BYTES = 12
    private const val TAG_BITS = 128

    private fun key(): SecretKey {
        val ks = KeyStore.getInstance(STORE).apply { load(null) }
        (ks.getKey(ALIAS, null) as? SecretKey)?.let { return it }
        val spec = KeyGenParameterSpec.Builder(ALIAS, KeyProperties.PURPOSE_ENCRYPT or KeyProperties.PURPOSE_DECRYPT)
            .setBlockModes(KeyProperties.BLOCK_MODE_GCM)
            .setEncryptionPaddings(KeyProperties.ENCRYPTION_PADDING_NONE)
            .setKeySize(256)
            // The key is for this app on this phone: not backed up (keystore keys never
            // are), and the OS may keep it in hardware where there is some.
            .build()
        return KeyGenerator.getInstance(KeyProperties.KEY_ALGORITHM_AES, STORE).run {
            init(spec)
            generateKey()
        }
    }

    @JvmStatic
    fun wrap(plain: ByteArray): ByteArray {
        val cipher = Cipher.getInstance("AES/GCM/NoPadding")
        cipher.init(Cipher.ENCRYPT_MODE, key())
        val iv = cipher.iv
        require(iv.size == IV_BYTES) { "unexpected GCM iv length ${iv.size}" }
        return iv + cipher.doFinal(plain)
    }

    @JvmStatic
    fun unwrap(wrapped: ByteArray): ByteArray {
        require(wrapped.size > IV_BYTES) { "wrapped key too short" }
        val cipher = Cipher.getInstance("AES/GCM/NoPadding")
        cipher.init(Cipher.DECRYPT_MODE, key(), GCMParameterSpec(TAG_BITS, wrapped, 0, IV_BYTES))
        return cipher.doFinal(wrapped, IV_BYTES, wrapped.size - IV_BYTES)
    }
}
