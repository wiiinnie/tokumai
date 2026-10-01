package com.tokumai.app

import android.os.Bundle
import android.view.View
import androidx.activity.enableEdgeToEdge
import androidx.core.view.ViewCompat
import androidx.core.view.WindowInsetsCompat
import androidx.core.view.updatePadding

class MainActivity : TauriActivity() {
  override fun onCreate(savedInstanceState: Bundle?) {
    enableEdgeToEdge()
    super.onCreate(savedInstanceState)
    // Edge-to-edge makes the window ignore the keyboard: the IME overlaid the composer
    // and hid what was being typed (emulator, 2026-08-30). Hand the IME inset to the
    // content view as bottom padding so the webview shrinks and the composer rides up.
    val root = findViewById<View>(android.R.id.content)
    ViewCompat.setOnApplyWindowInsetsListener(root) { v, insets ->
      val ime = insets.getInsets(WindowInsetsCompat.Type.ime())
      v.updatePadding(bottom = ime.bottom)
      insets
    }
  }
}
