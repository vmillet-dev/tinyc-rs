package dev.millet.tinyc.lang

import com.intellij.openapi.fileTypes.LanguageFileType
import javax.swing.Icon

object TinyCFileType : LanguageFileType(TinyCLanguage) {
    const val EXTENSION: String = "tc"

    override fun getName(): String = "TinyC"
    override fun getDescription(): String = "TinyC source file"
    override fun getDefaultExtension(): String = EXTENSION
    override fun getIcon(): Icon = TinyCIcons.FILE
}
