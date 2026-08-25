package dev.millet.tinyc.settings

import com.intellij.openapi.application.ApplicationManager
import com.intellij.openapi.components.PersistentStateComponent
import com.intellij.openapi.components.Service
import com.intellij.openapi.components.State
import com.intellij.openapi.components.Storage
import com.intellij.openapi.components.service
import com.intellij.openapi.fileChooser.FileChooserDescriptor
import com.intellij.openapi.fileChooser.FileChooserDescriptorFactory
import com.intellij.openapi.options.Configurable
import com.intellij.openapi.project.Project
import com.intellij.openapi.ui.TextFieldWithBrowseButton
import com.intellij.ui.JBColor
import com.intellij.ui.components.JBCheckBox
import com.intellij.ui.components.JBLabel
import com.intellij.util.ui.FormBuilder
import com.intellij.util.ui.UIUtil
import com.intellij.util.xmlb.XmlSerializerUtil
import dev.millet.tinyc.toolchain.TinyCToolchain
import javax.swing.JButton
import javax.swing.JComponent
import javax.swing.JPanel

/**
 * What the plugin needs told, which is as little as possible: every field here
 * is a path the plugin would otherwise go looking for, and finding it is the
 * default. A machine with `tinyc`, `nasm` and a linker in the usual places
 * needs none of them filled in — and one without them needs no project, only
 * these.
 */
class TinyCSettingsState {
    /** The `tinyc` executable. Empty: a built one, then `PATH`, then `cargo run`. */
    @JvmField var compilerPath: String = ""

    /** NASM. Empty: `PATH`, then the places `winget install nasm` uses. */
    @JvmField var nasmPath: String = ""

    /** `link.exe`, or the C compiler to link with. Empty: found. */
    @JvmField var linkerPath: String = ""

    /** `vcvars64.bat`, which is what tells `link.exe` where `LIB` is. */
    @JvmField var msvcEnvironmentPath: String = ""

    /** Whether the compiler is run to mark mistakes while typing. */
    @JvmField var annotate: Boolean = true

    /** Whether the optimiser runs for the diagnostics shown in the editor. */
    @JvmField var optimiseWhileAnnotating: Boolean = false
}

@Service(Service.Level.PROJECT)
@State(name = "TinyCSettings", storages = [Storage("tinyc.xml")])
class TinyCSettings : PersistentStateComponent<TinyCSettingsState> {
    private var settings = TinyCSettingsState()

    /**
     * The values themselves. Named apart from [getState], which belongs to the
     * interface and so cannot also be a property.
     */
    val options: TinyCSettingsState get() = settings

    override fun getState(): TinyCSettingsState = settings

    override fun loadState(state: TinyCSettingsState) {
        XmlSerializerUtil.copyBean(state, settings)
    }

    companion object {
        fun of(project: Project): TinyCSettings = project.service()
    }
}

class TinyCConfigurable(private val project: Project) : Configurable {
    private val compiler = TextFieldWithBrowseButton()
    private val nasm = TextFieldWithBrowseButton()
    private val linker = TextFieldWithBrowseButton()
    private val msvcEnvironment = TextFieldWithBrowseButton()
    private val annotate = JBCheckBox("Report the compiler's diagnostics while typing")
    private val optimise = JBCheckBox("Run the optimiser for those diagnostics")
    private val found = JBLabel("")

    override fun getDisplayName(): String = "TinyC"

    override fun createComponent(): JComponent {
        browse(compiler, "tinyc Executable")
        browse(nasm, "NASM Executable")
        browse(linker, if (isWindows) "Microsoft Linker (link.exe)" else "C Compiler to Link With")
        browse(msvcEnvironment, "MSVC Environment (vcvars64.bat)")

        val detect = JButton("What would be used?")
        detect.addActionListener { describe() }

        val builder = FormBuilder.createFormBuilder()
            .addLabeledComponent(JBLabel("tinyc:"), compiler, 1, false)
            .addComponentToRightColumn(
                comment(
                    "Empty: a target/release or target/debug binary of the TinyC repository above the " +
                        "file, then PATH, then cargo run.",
                ),
                1,
            )
            .addLabeledComponent(JBLabel("nasm:"), nasm, 1, false)
            .addComponentToRightColumn(comment("Empty: PATH, then where winget install nasm puts it."), 1)
            .addLabeledComponent(JBLabel(if (isWindows) "link.exe:" else "Linker:"), linker, 1, false)
            .addComponentToRightColumn(
                comment(
                    if (isWindows) {
                        "Empty: the one that comes with the Visual Studio installation below."
                    } else {
                        "Empty: the first of cc, gcc or clang on PATH — a C compiler links, because it " +
                            "is what knows where the C library is."
                    },
                ),
                1,
            )

        if (isWindows) {
            builder
                .addLabeledComponent(JBLabel("vcvars64.bat:"), msvcEnvironment, 1, false)
                .addComponentToRightColumn(
                    comment(
                        "Empty: found through vswhere. It is what sets LIB, without which link.exe " +
                            "cannot find the C runtime.",
                    ),
                    1,
                )
        }

        val panel = builder
            .addComponent(annotate, 12)
            .addComponent(optimise, 1)
            .addComponentToRightColumn(
                comment("Off by default: a pass may not change what a program means, so this only costs time."),
                1,
            )
            .addComponent(detect, 12)
            .addComponent(found, 1)
            .addComponentFillVertically(JPanel(), 0)
            .panel

        reset()
        return panel
    }

    /**
     * Ask the toolchain what it would pick — with the fields as they are on
     * screen rather than as they were last saved, since the point of the button
     * is to try a path before applying it.
     *
     * Off the dialog's thread: reading the MSVC environment means running a
     * `cmd`, which takes a second the first time.
     */
    private fun describe() {
        found.text = "looking…"
        val settings = TinyCSettings.of(project)
        val onScreen = TinyCSettingsState().also { copyInto(it) }
        val saved = TinyCSettingsState().also { XmlSerializerUtil.copyBean(settings.options, it) }

        ApplicationManager.getApplication().executeOnPooledThread {
            val text = try {
                XmlSerializerUtil.copyBean(onScreen, settings.options)
                TinyCToolchain.of(project).describe()
            } finally {
                XmlSerializerUtil.copyBean(saved, settings.options)
            }
            ApplicationManager.getApplication().invokeLater {
                found.text = "<html>" + text.replace("<", "&lt;").replace("\n", "<br>") + "</html>"
            }
        }
    }

    private fun browse(field: TextFieldWithBrowseButton, title: String) {
        field.addBrowseFolderListener(project, descriptor().withTitle(title))
    }

    private fun descriptor(): FileChooserDescriptor = FileChooserDescriptorFactory.singleFile()

    private fun comment(text: String): JBLabel {
        val label = JBLabel(text)
        label.componentStyle = UIUtil.ComponentStyle.SMALL
        label.foreground = JBColor.GRAY
        return label
    }

    private fun copyInto(state: TinyCSettingsState) {
        state.compilerPath = compiler.text.trim()
        state.nasmPath = nasm.text.trim()
        state.linkerPath = linker.text.trim()
        state.msvcEnvironmentPath = msvcEnvironment.text.trim()
        state.annotate = annotate.isSelected
        state.optimiseWhileAnnotating = optimise.isSelected
    }

    override fun isModified(): Boolean {
        val state = TinyCSettings.of(project).options
        return compiler.text.trim() != state.compilerPath ||
            nasm.text.trim() != state.nasmPath ||
            linker.text.trim() != state.linkerPath ||
            msvcEnvironment.text.trim() != state.msvcEnvironmentPath ||
            annotate.isSelected != state.annotate ||
            optimise.isSelected != state.optimiseWhileAnnotating
    }

    override fun apply() = copyInto(TinyCSettings.of(project).options)

    override fun reset() {
        val state = TinyCSettings.of(project).options
        compiler.text = state.compilerPath
        nasm.text = state.nasmPath
        linker.text = state.linkerPath
        msvcEnvironment.text = state.msvcEnvironmentPath
        annotate.isSelected = state.annotate
        optimise.isSelected = state.optimiseWhileAnnotating
    }

    private val isWindows: Boolean
        get() = System.getProperty("os.name").lowercase().startsWith("win")
}
