package dev.millet.tinyc.run

import com.intellij.openapi.fileChooser.FileChooserDescriptorFactory
import com.intellij.openapi.options.SettingsEditor
import com.intellij.openapi.project.Project
import com.intellij.openapi.ui.TextFieldWithBrowseButton
import com.intellij.ui.JBColor
import com.intellij.ui.components.JBCheckBox
import com.intellij.ui.components.JBLabel
import com.intellij.ui.components.JBTextField
import com.intellij.util.ui.FormBuilder
import com.intellij.util.ui.UIUtil
import javax.swing.JComponent
import javax.swing.JPanel

class TinyCRunConfigurationEditor(private val project: Project) : SettingsEditor<TinyCRunConfiguration>() {
    private val source = TextFieldWithBrowseButton()
    private val workingDirectory = TextFieldWithBrowseButton()
    private val outputDirectory = JBTextField()
    private val compilerArguments = JBTextField()
    private val buildOnly = JBCheckBox("Build only, do not run")

    override fun resetEditorFrom(configuration: TinyCRunConfiguration) {
        source.text = configuration.sourceFile
        workingDirectory.text = configuration.workingDirectory
        outputDirectory.text = configuration.outputDirectory
        compilerArguments.text = configuration.compilerArguments
        buildOnly.isSelected = configuration.buildOnly
    }

    override fun applyEditorTo(configuration: TinyCRunConfiguration) {
        configuration.sourceFile = source.text.trim()
        configuration.workingDirectory = workingDirectory.text.trim()
        configuration.outputDirectory = outputDirectory.text.trim().ifEmpty { "out" }
        configuration.compilerArguments = compilerArguments.text.trim()
        configuration.buildOnly = buildOnly.isSelected
    }

    override fun createEditor(): JComponent {
        source.addBrowseFolderListener(
            project,
            FileChooserDescriptorFactory.createSingleFileDescriptor("tc").withTitle("TinyC Source File"),
        )
        workingDirectory.addBrowseFolderListener(
            project,
            FileChooserDescriptorFactory.createSingleFolderDescriptor().withTitle("Working Directory"),
        )

        return FormBuilder.createFormBuilder()
            .addLabeledComponent(JBLabel("Source file:"), source, 1, false)
            .addLabeledComponent(JBLabel("Working directory:"), workingDirectory, 1, false)
            .addComponentToRightColumn(
                comment("Empty: the TinyC repository above the file, else the directory it is in."),
                1,
            )
            .addLabeledComponent(JBLabel("Output directory:"), outputDirectory, 1, false)
            .addComponentToRightColumn(comment("The .asm, the object file and the executable. Relative to the above."), 1)
            .addLabeledComponent(JBLabel("Compiler arguments:"), compilerArguments, 1, false)
            .addComponentToRightColumn(comment("Passed to tinyc — --no-optimise, --target x86_64-linux, and so on."), 1)
            .addComponent(buildOnly, 12)
            .addComponentFillVertically(JPanel(), 0)
            .panel
    }

    private fun comment(text: String): JBLabel {
        val label = JBLabel(text)
        label.componentStyle = UIUtil.ComponentStyle.SMALL
        label.foreground = JBColor.GRAY
        return label
    }
}
