package dev.millet.tinyc.run

import com.intellij.execution.ExecutionException
import com.intellij.execution.Executor
import com.intellij.execution.configurations.CommandLineState
import com.intellij.execution.configurations.ConfigurationFactory
import com.intellij.execution.configurations.ConfigurationTypeBase
import com.intellij.execution.configurations.GeneralCommandLine
import com.intellij.execution.configurations.LocatableConfigurationBase
import com.intellij.execution.configurations.RunConfiguration
import com.intellij.execution.configurations.RunConfigurationOptions
import com.intellij.execution.configurations.RunProfileState
import com.intellij.execution.configurations.RuntimeConfigurationError
import com.intellij.execution.process.KillableColoredProcessHandler
import com.intellij.execution.process.ProcessHandler
import com.intellij.execution.process.ProcessTerminatedListener
import com.intellij.execution.runners.ExecutionEnvironment
import com.intellij.icons.AllIcons
import com.intellij.openapi.options.SettingsEditor
import com.intellij.openapi.project.Project
import com.intellij.openapi.util.JDOMExternalizerUtil
import dev.millet.tinyc.toolchain.TinyCToolchain
import dev.millet.tinyc.toolchain.TinyCTooling
import org.jdom.Element
import java.nio.file.Files
import java.nio.file.Path

/**
 * "Run a TinyC program", which means the whole way: source, assembly, object,
 * executable, output in the console.
 *
 * The three tools are found or configured (Settings | Tools | TinyC) and driven
 * from here — see [TinyCBuild] — so nothing about this needs the TinyC
 * repository to be the open project, or to be anywhere at all. A `.tc` file on
 * a memory stick builds, as long as the plugin knows where `tinyc` is.
 */
class TinyCRunConfiguration(
    project: Project,
    factory: ConfigurationFactory,
    name: String,
) : LocatableConfigurationBase<RunConfigurationOptions>(project, factory, name) {

    var sourceFile: String = ""

    /** Where the tools run, and what the source is named relative to. Empty: found. */
    var workingDirectory: String = ""

    /** Where the `.asm`, the object and the executable go. Relative to the above. */
    var outputDirectory: String = "out"

    /** Anything extra for the compiler — `--no-optimise`, say. */
    var compilerArguments: String = ""

    var buildOnly: Boolean = false

    override fun getConfigurationEditor(): SettingsEditor<out RunConfiguration> = TinyCRunConfigurationEditor(project)

    override fun checkConfiguration() {
        if (sourceFile.isBlank()) throw RuntimeConfigurationError("Choose a .tc file to run")
        if (!Files.isRegularFile(Path.of(sourceFile))) {
            throw RuntimeConfigurationError("$sourceFile does not exist")
        }
        // Where the tools are is deliberately *not* checked here: finding the
        // MSVC environment means running a `cmd`, and this is asked on every
        // keystroke in the dialog. A missing tool is reported by the run.
    }

    /**
     * Where everything is run from: what was asked for, else the TinyC
     * repository above the file — which keeps the old `out/` in the same place
     * for anyone working inside it — else the directory the file is in.
     */
    fun workingDirectoryFor(source: Path): Path = workingDirectory.takeIf { it.isNotBlank() }
        ?.let { Path.of(it) }
        ?: TinyCTooling.repositoryAbove(source)
        ?: source.toAbsolutePath().parent

    fun outputDirectoryFor(from: Path): Path {
        val named = outputDirectory.ifBlank { "out" }
        val path = Path.of(named)
        return if (path.isAbsolute) path else from.resolve(path)
    }

    override fun getState(executor: Executor, environment: ExecutionEnvironment): RunProfileState {
        val source = Path.of(sourceFile).toAbsolutePath()
        val from = workingDirectoryFor(source)
        val output = outputDirectoryFor(from)
        val arguments = compilerArguments.split(' ').map { it.trim() }.filter { it.isNotEmpty() }

        return object : CommandLineState(environment) {
            init {
                addConsoleFilters(TinyCConsoleFilter(project, from))
            }

            override fun startProcess(): ProcessHandler {
                val resolved = TinyCToolchain.of(project).resolve(source)
                val tools = when (resolved) {
                    is TinyCToolchain.Resolution.Missing -> throw ExecutionException(resolved.message)
                    is TinyCToolchain.Resolution.Found -> resolved.tools
                }

                val outcome = TinyCBuild.build(tools, source, output, from, arguments)
                if (!outcome.built) return TinyCMessageProcessHandler(outcome.log, failed = true)
                val executable = outcome.executable
                if (buildOnly || executable == null) return TinyCMessageProcessHandler(outcome.log, failed = false)

                val command = GeneralCommandLine(executable.toString())
                    .withWorkingDirectory(from)
                    .withCharset(Charsets.UTF_8)

                val handler = object : KillableColoredProcessHandler(command) {
                    override fun startNotify() {
                        // The build happened before this console existed, so its
                        // log is handed over first — the run then reads as one
                        // story, exactly as the scripts print it in a terminal.
                        notifyTextAvailable(outcome.log + "==> running\n", com.intellij.execution.process.ProcessOutputTypes.SYSTEM)
                        super.startNotify()
                    }
                }
                ProcessTerminatedListener.attach(handler, project)
                return handler
            }
        }
    }

    override fun writeExternal(element: Element) {
        super.writeExternal(element)
        JDOMExternalizerUtil.writeField(element, "sourceFile", sourceFile)
        JDOMExternalizerUtil.writeField(element, "workingDirectory", workingDirectory)
        JDOMExternalizerUtil.writeField(element, "outputDirectory", outputDirectory)
        JDOMExternalizerUtil.writeField(element, "compilerArguments", compilerArguments)
        JDOMExternalizerUtil.writeField(element, "buildOnly", buildOnly.toString())
    }

    override fun readExternal(element: Element) {
        super.readExternal(element)
        sourceFile = JDOMExternalizerUtil.readField(element, "sourceFile", "")
        workingDirectory = JDOMExternalizerUtil.readField(element, "workingDirectory", "")
        outputDirectory = JDOMExternalizerUtil.readField(element, "outputDirectory", "out")
        compilerArguments = JDOMExternalizerUtil.readField(element, "compilerArguments", "")
        buildOnly = JDOMExternalizerUtil.readField(element, "buildOnly", "false").toBoolean()
    }

    override fun suggestedName(): String =
        sourceFile.takeIf { it.isNotBlank() }?.let { Path.of(it).fileName?.toString() } ?: "TinyC"
}

class TinyCRunConfigurationType : ConfigurationTypeBase(
    ID,
    "TinyC",
    "Compile a TinyC program and run it",
    AllIcons.RunConfigurations.Application,
) {
    init {
        addFactory(object : ConfigurationFactory(this) {
            override fun getId(): String = ID
            override fun createTemplateConfiguration(project: Project): RunConfiguration =
                TinyCRunConfiguration(project, this, "TinyC")
        })
    }

    companion object {
        const val ID = "TinyCRunConfiguration"

        fun factory(): ConfigurationFactory =
            com.intellij.execution.configurations.ConfigurationTypeUtil
                .findConfigurationType(TinyCRunConfigurationType::class.java)
                .configurationFactories
                .first()
    }
}
