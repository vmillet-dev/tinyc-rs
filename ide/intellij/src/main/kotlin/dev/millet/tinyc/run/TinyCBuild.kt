package dev.millet.tinyc.run

import dev.millet.tinyc.toolchain.TinyCLinker
import dev.millet.tinyc.toolchain.TinyCTooling
import dev.millet.tinyc.toolchain.TinyCTools
import java.nio.file.Files
import java.nio.file.Path
import java.util.concurrent.TimeUnit

/**
 * Source to executable, in three steps.
 *
 * ```text
 * tinyc  source.tc  -> source.asm
 * nasm   source.asm -> source.obj   (-f win64, or elf64 elsewhere)
 * link   source.obj -> source.exe   (against the C runtime, for printf)
 * ```
 *
 * This is what `scripts/build.ps1` and `scripts/build.sh` do, said again here so
 * that the IDE needs neither of them — and so that a `.tc` file anywhere on the
 * disk can be built from a plugin that was told where the three tools are. The
 * repetition is real and is the price of that independence; `BuildMatchesTheScriptsTest`
 * is what keeps the two from drifting, by reading the flags back out of the
 * scripts and comparing them with the constants below.
 *
 * There is no `Project` and no IntelliJ API in here on purpose: it can be run
 * from a plain test, and it is.
 */
object TinyCBuild {

    /**
     * `/entry:mainCRTStartup` because the C runtime's startup is what calls
     * `main`, and `/subsystem:console` because a TinyC program writes to one.
     */
    val MSVC_ARGUMENTS = listOf("/nologo", "/subsystem:console", "/entry:mainCRTStartup")

    /**
     * `msvcrt.lib` is the C runtime (`printf` lives there), `kernel32.lib` has
     * `SetConsoleOutputCP` so a console reads what is printed as UTF-8, and
     * `legacy_stdio_definitions.lib` exports `printf` as a real symbol rather
     * than the inline function the UCRT headers otherwise provide.
     */
    val MSVC_LIBRARIES = listOf("msvcrt.lib", "kernel32.lib", "legacy_stdio_definitions.lib")

    /**
     * `-no-pie`, because a position-independent executable reaches every symbol
     * through the GOT or the PLT and assembly that names them outright does not;
     * `-lpthread` for `pthread_getattr_np`, which is how the prologue's stack
     * check finds out where the stack ends.
     */
    val CC_ARGUMENTS = listOf("-no-pie")
    val CC_LIBRARIES = listOf("-lpthread")

    /** What a step announces itself with, the way the scripts do. */
    private const val ARROW = "==> "

    private const val TIMEOUT_MINUTES = 5L

    data class Outcome(
        val built: Boolean,
        val log: String,
        val executable: Path?,
        val assembly: Path?,
    )

    /**
     * Build [source] into [outputDirectory]. Every tool is run from
     * [workingDirectory], which is also what the source is named relative to —
     * a short path is what makes the compiler's `--> file:line:column` a link in
     * the console.
     */
    fun build(
        tools: TinyCTools,
        source: Path,
        outputDirectory: Path,
        workingDirectory: Path,
        compilerArguments: List<String> = emptyList(),
    ): Outcome {
        val log = StringBuilder()
        val name = source.fileName.toString().substringBeforeLast('.')
        val assembly = outputDirectory.resolve("$name.asm")
        val obj = outputDirectory.resolve(if (TinyCTooling.isWindows) "$name.obj" else "$name.o")
        val executable = outputDirectory.resolve(if (TinyCTooling.isWindows) "$name.exe" else name)

        try {
            Files.createDirectories(outputDirectory)
        } catch (e: Exception) {
            log.append("cannot create $outputDirectory: ${e.message}\n")
            return Outcome(false, log.toString(), null, null)
        }

        val named = relative(source, workingDirectory)

        val compiled = step(
            log,
            "tinyc $named",
            tools.compiler + compilerArguments + listOf(named, "-o", assembly.toString()),
            workingDirectory,
            null,
        )
        if (!compiled) return failed(log, "compiling failed", assembly)

        val assembled = step(
            log,
            "nasm",
            listOf(tools.nasm.toString(), "-f", TinyCTooling.objectFormat, "-o", obj.toString(), assembly.toString()),
            workingDirectory,
            null,
        )
        if (!assembled) return failed(log, "assembling failed", assembly)

        val linker = tools.linker
        val linked = when (linker) {
            is TinyCLinker.Msvc -> step(
                log,
                "link",
                listOf(linker.executable.toString()) + MSVC_ARGUMENTS +
                    listOf("/out:$executable", obj.toString()) + MSVC_LIBRARIES,
                workingDirectory,
                linker.environment,
            )

            is TinyCLinker.Cc -> step(
                log,
                linker.executable.fileName.toString(),
                listOf(linker.executable.toString()) + CC_ARGUMENTS +
                    listOf(obj.toString(), "-o", executable.toString()) + CC_LIBRARIES,
                workingDirectory,
                null,
            )
        }
        if (!linked) return failed(log, "linking failed", assembly)

        log.append(ARROW).append("built ").append(executable).append('\n')
        return Outcome(true, log.toString(), executable, assembly)
    }

    private fun failed(log: StringBuilder, why: String, assembly: Path?): Outcome {
        log.append(ARROW).append(why).append('\n')
        return Outcome(false, log.toString(), null, assembly.takeIf { it != null && Files.exists(it) })
    }

    /**
     * One tool, run to completion, everything it printed kept in order.
     *
     * `stderr` is folded into `stdout` because the two together are the story:
     * a diagnostic and the line it belongs under must not arrive interleaved by
     * chance.
     */
    private fun step(
        log: StringBuilder,
        headline: String,
        command: List<String>,
        workingDirectory: Path,
        environment: Map<String, String>?,
    ): Boolean {
        log.append(ARROW).append(headline).append('\n')
        return try {
            val builder = ProcessBuilder(command)
                .directory(workingDirectory.toFile())
                .redirectErrorStream(true)
            if (environment != null) apply(builder, environment)

            val process = builder.start()
            val printed = process.inputStream.readBytes().toString(Charsets.UTF_8)
            val finished = process.waitFor(TIMEOUT_MINUTES, TimeUnit.MINUTES)
            log.append(printed)
            if (!finished) {
                process.destroyForcibly()
                log.append("gave up waiting after $TIMEOUT_MINUTES minutes\n")
                false
            } else {
                process.exitValue() == 0
            }
        } catch (e: Exception) {
            log.append(command.first()).append(": ").append(e.message).append('\n')
            false
        }
    }

    /**
     * Put an environment on top of the inherited one.
     *
     * Windows treats `PATH` and `Path` as one variable and `ProcessBuilder` does
     * not, so a name already there under another spelling is taken out first —
     * otherwise the process gets both, and which one it reads is anyone's guess.
     */
    private fun apply(builder: ProcessBuilder, environment: Map<String, String>) {
        val existing = builder.environment()
        for (name in environment.keys) {
            existing.keys.filter { it.equals(name, ignoreCase = true) }.forEach { existing.remove(it) }
        }
        existing.putAll(environment)
    }

    /** [source] as written from [directory], when it lives under it. */
    private fun relative(source: Path, directory: Path): String {
        val absolute = source.toAbsolutePath().normalize()
        val from = directory.toAbsolutePath().normalize()
        return if (absolute.startsWith(from)) from.relativize(absolute).toString() else absolute.toString()
    }
}
