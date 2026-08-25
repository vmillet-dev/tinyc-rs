package dev.millet.tinyc.toolchain

import java.nio.file.Files
import java.nio.file.Path
import java.util.concurrent.ConcurrentHashMap
import java.util.concurrent.TimeUnit

/**
 * The three tools a TinyC program needs, and where they are.
 *
 * `tinyc` writes assembly and stops there, so an executable takes an assembler
 * and a linker as well. Each of the three may be *said* — in Settings | Tools |
 * TinyC — and each is *looked for* when it is not, so a machine that has them in
 * the usual places needs nothing filled in.
 *
 * Nothing in here knows about a project, a `Project`, or the TinyC repository:
 * it is plain path lookup, which is what makes it testable and what lets the
 * plugin build a `.tc` file that lives anywhere at all.
 */
object TinyCTooling {

    val isWindows: Boolean = System.getProperty("os.name").lowercase().startsWith("win")

    /** What NASM should be told to produce for this machine. */
    val objectFormat: String get() = if (isWindows) "win64" else "elf64"

    // -- the compiler -------------------------------------------------------

    /**
     * How to run `tinyc`, and where from.
     *
     * In order: what the settings say; the binary a TinyC repository above the
     * file has already built; the one on `PATH`; and finally `cargo run` in that
     * repository, which is what makes a fresh clone work with nothing installed
     * and nothing configured.
     */
    fun findCompiler(explicit: String?, near: Path?): List<String>? {
        executable(explicit)?.let { return listOf(it.toString()) }

        val repository = repositoryAbove(near)
        if (repository != null) {
            for (profile in listOf("release", "debug")) {
                val built = repository.resolve("target/$profile/$TINYC")
                if (Files.isExecutable(built)) return listOf(built.toString())
            }
        }

        onPath(TINYC)?.let { return listOf(it.toString()) }

        if (repository != null && onPath(if (isWindows) "cargo.exe" else "cargo") != null) {
            return listOf(
                "cargo", "run", "--quiet",
                "--manifest-path", repository.resolve("Cargo.toml").toString(),
                "--",
            )
        }
        return null
    }

    /** The directory of a TinyC checkout above [start], if there is one. */
    fun repositoryAbove(start: Path?): Path? {
        var candidate = start?.toAbsolutePath()?.parent
        while (candidate != null) {
            if (Files.isRegularFile(candidate.resolve("Cargo.toml")) &&
                Files.isRegularFile(candidate.resolve("src/main.rs"))
            ) {
                return candidate
            }
            candidate = candidate.parent
        }
        return null
    }

    // -- the assembler ------------------------------------------------------

    /**
     * NASM. `winget install nasm` does not put it on `PATH`, so the places it
     * normally lands are tried too — the same list `scripts/build.ps1` has.
     */
    fun findNasm(explicit: String?): Path? {
        executable(explicit)?.let { return it }
        onPath(if (isWindows) "nasm.exe" else "nasm")?.let { return it }
        if (!isWindows) return null
        return listOfNotNull(
            environmentPath("LOCALAPPDATA", "bin/NASM/nasm.exe"),
            environmentPath("ProgramFiles", "NASM/nasm.exe"),
            environmentPath("ProgramFiles(x86)", "NASM/nasm.exe"),
        ).firstOrNull { Files.isExecutable(it) }
    }

    // -- the linker ---------------------------------------------------------

    /**
     * The linker, and on Windows the environment it needs.
     *
     * `link.exe` cannot be run out of thin air: it finds the C runtime through
     * `LIB`, which a developer command prompt sets. So the environment
     * `vcvars64.bat` produces is captured once and handed to the process,
     * rather than launching everything through `cmd /c call ... && link` —
     * which is the same bargain, made once instead of every build.
     */
    fun findLinker(explicitLinker: String?, explicitEnvironment: String?): TinyCLinker? {
        if (!isWindows) {
            val cc = executable(explicitLinker)
                ?: listOf("cc", "gcc", "clang").firstNotNullOfOrNull { onPath(it) }
                ?: return null
            return TinyCLinker.Cc(cc)
        }

        val environment = findVcvars(explicitEnvironment)?.let { msvcEnvironment(it) } ?: emptyMap()
        executable(explicitLinker)?.let { return TinyCLinker.Msvc(it, environment) }

        // `link` is also the name of a coreutils program, and a machine with Git
        // for Windows on it has that one on PATH. So the search starts with the
        // directories `vcvars64.bat` *added*, and whatever it finds has to have
        // `cl.exe` beside it — which Microsoft's linker does and no other does.
        val link = firstIn(entriesAddedBy(environment), "link.exe")
            ?: firstIn(entries(environment["Path"].orEmpty()), "link.exe")
            ?: firstIn(entries(System.getenv("PATH").orEmpty()), "link.exe")
            ?: return null
        return TinyCLinker.Msvc(link, environment)
    }

    /** `vcvars64.bat`, from the Visual Studio installation `vswhere` names. */
    fun findVcvars(explicit: String?): Path? {
        explicit?.takeIf { it.isNotBlank() }?.let { named ->
            val path = Path.of(named)
            if (Files.isRegularFile(path)) return path
        }
        val vswhere = environmentPath("ProgramFiles(x86)", "Microsoft Visual Studio/Installer/vswhere.exe")
            ?: return null
        if (!Files.isExecutable(vswhere)) return null

        val installation = capture(
            listOf(vswhere.toString(), "-latest", "-products", "*", "-property", "installationPath"),
        )?.trim()?.lineSequence()?.firstOrNull()?.trim() ?: return null
        if (installation.isEmpty()) return null

        val vcvars = Path.of(installation, "VC", "Auxiliary", "Build", "vcvars64.bat")
        return vcvars.takeIf { Files.isRegularFile(it) }
    }

    /**
     * Everything `vcvars64.bat` sets, read back out of a `cmd` that ran it.
     *
     * Kept for as long as the IDE runs: it costs a second or two, and a
     * Visual Studio installation does not move between two builds.
     */
    fun msvcEnvironment(vcvars: Path): Map<String, String> = msvcEnvironments.getOrPut(vcvars.toString()) {
        val printed = capture(listOf("cmd.exe", "/c", "call \"$vcvars\" >nul 2>&1 && set")) ?: return@getOrPut emptyMap()
        printed.lineSequence()
            .mapNotNull { line ->
                val at = line.indexOf('=')
                if (at <= 0) null else line.substring(0, at) to line.substring(at + 1).trim()
            }
            .toMap()
    }

    // -- odds and ends ------------------------------------------------------

    /** A named executable on `PATH`, or nothing. */
    fun onPath(name: String): Path? = entries(System.getenv("PATH").orEmpty())
        .asSequence()
        .mapNotNull { runCatching { Path.of(it, name) }.getOrNull() }
        .firstOrNull { Files.isExecutable(it) && !Files.isDirectory(it) }

    private fun entries(paths: String): List<String> = paths
        .split(java.io.File.pathSeparatorChar)
        .map { it.trim().trim('"') }
        .filter { it.isNotEmpty() }

    /** The directories an environment has that the inherited one has not. */
    private fun entriesAddedBy(environment: Map<String, String>): List<String> {
        val inherited = entries(System.getenv("PATH").orEmpty()).map { it.lowercase() }.toSet()
        return entries(environment["Path"] ?: environment["PATH"].orEmpty())
            .filter { it.lowercase() !in inherited }
    }

    /**
     * The first `link.exe` among [directories] that is Microsoft's.
     *
     * `cl.exe` beside it is the whole test: the compiler and the linker ship in
     * the same directory, and nothing else that is called `link` does.
     */
    private fun firstIn(directories: List<String>, name: String): Path? = directories
        .asSequence()
        .mapNotNull { runCatching { Path.of(it, name) }.getOrNull() }
        .filter { Files.isExecutable(it) && !Files.isDirectory(it) }
        .firstOrNull { Files.isRegularFile(it.resolveSibling("cl.exe")) }

    private fun executable(named: String?): Path? = named
        ?.takeIf { it.isNotBlank() }
        ?.let { runCatching { Path.of(it) }.getOrNull() }
        ?.takeIf { Files.isExecutable(it) && !Files.isDirectory(it) }

    private fun environmentPath(variable: String, relative: String): Path? =
        System.getenv(variable)?.takeIf { it.isNotBlank() }?.let { Path.of(it).resolve(relative) }

    /** Run something small and answer what it printed, or nothing if it failed. */
    private fun capture(command: List<String>): String? = try {
        val process = ProcessBuilder(command).redirectErrorStream(true).start()
        val text = process.inputStream.readBytes().toString(Charsets.UTF_8)
        if (!process.waitFor(30, TimeUnit.SECONDS)) {
            process.destroyForcibly()
            null
        } else if (process.exitValue() == 0) {
            text
        } else {
            null
        }
    } catch (_: Exception) {
        null
    }

    private val msvcEnvironments = ConcurrentHashMap<String, Map<String, String>>()

    private val TINYC = if (isWindows) "tinyc.exe" else "tinyc"
}

sealed interface TinyCLinker {
    val executable: Path

    /** Microsoft's linker, with the environment that tells it where `LIB` is. */
    data class Msvc(override val executable: Path, val environment: Map<String, String>) : TinyCLinker

    /** A C compiler, used as the linker because it knows where the C library is. */
    data class Cc(override val executable: Path) : TinyCLinker
}

/** The three tools, all found. */
data class TinyCTools(
    /** The command that runs the compiler, before any argument of ours. */
    val compiler: List<String>,
    val nasm: Path,
    val linker: TinyCLinker,
)
