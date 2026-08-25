package dev.millet.tinyc.toolchain

import com.intellij.execution.configurations.GeneralCommandLine
import com.intellij.openapi.components.Service
import com.intellij.openapi.components.service
import com.intellij.openapi.project.Project
import dev.millet.tinyc.settings.TinyCSettings
import java.nio.file.Path

/**
 * The tools, as this project is configured to see them.
 *
 * All the finding is [TinyCTooling]'s, which knows nothing about IntelliJ; this
 * only puts the settings in front of it and turns "not found" into a sentence
 * that says what to install or what to fill in.
 */
@Service(Service.Level.PROJECT)
class TinyCToolchain(private val project: Project) {

    private val settings get() = TinyCSettings.of(project).options

    /** Either all three tools, or the reason there are not three. */
    sealed interface Resolution {
        data class Found(val tools: TinyCTools) : Resolution
        data class Missing(val message: String) : Resolution
    }

    fun compiler(near: Path? = null): List<String>? =
        TinyCTooling.findCompiler(settings.compilerPath, near)

    /**
     * A command line that runs the compiler, with no arguments yet — what the
     * editor uses to ask for diagnostics.
     */
    fun compilerCommand(near: Path? = null): GeneralCommandLine? {
        val command = compiler(near) ?: return null
        val line = GeneralCommandLine(command)
        val directory = TinyCTooling.repositoryAbove(near) ?: near?.parent
        if (directory != null) line.withWorkingDirectory(directory)
        return line
    }

    fun nasm(): Path? = TinyCTooling.findNasm(settings.nasmPath)

    fun linker(): TinyCLinker? = TinyCTooling.findLinker(settings.linkerPath, settings.msvcEnvironmentPath)

    fun resolve(near: Path? = null): Resolution {
        val compiler = compiler(near)
        val nasm = nasm()
        val linker = linker()
        val missing = ArrayList<String>()
        if (compiler == null) missing += COMPILER_MISSING
        if (nasm == null) missing += NASM_MISSING
        if (linker == null) missing += if (TinyCTooling.isWindows) MSVC_MISSING else CC_MISSING
        if (missing.isNotEmpty()) {
            return Resolution.Missing(
                missing.joinToString("\n") + "\n\nSet them in Settings | Tools | TinyC.",
            )
        }
        return Resolution.Found(TinyCTools(compiler!!, nasm!!, linker!!))
    }

    /** What the settings page shows when asked what it would use. */
    fun describe(near: Path? = null): String {
        val lines = ArrayList<String>()
        lines += "tinyc: " + (compiler(near)?.joinToString(" ") ?: "not found")
        lines += "nasm: " + (nasm()?.toString() ?: "not found")
        lines += when (val linker = linker()) {
            is TinyCLinker.Msvc -> "linker: ${linker.executable}" +
                if (linker.environment.isEmpty()) " (no MSVC environment: LIB may be missing)" else ""

            is TinyCLinker.Cc -> "linker: ${linker.executable}"
            null -> "linker: not found"
        }
        return lines.joinToString("\n")
    }

    companion object {
        private const val COMPILER_MISSING =
            "tinyc was not found: build it (cargo build --release) or name the executable."
        private const val NASM_MISSING =
            "nasm was not found: install it (winget install nasm, or apt install nasm) or name it."
        private const val MSVC_MISSING =
            "link.exe was not found: install Visual Studio with the \"Desktop development with C++\" " +
                "workload, or name link.exe and its vcvars64.bat."
        private const val CC_MISSING =
            "no C compiler was found to link with: install one (apt install build-essential) or name it."

        fun of(project: Project): TinyCToolchain = project.service()
    }
}
