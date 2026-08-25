import org.jetbrains.intellij.platform.gradle.TestFrameworkType
import org.jetbrains.kotlin.gradle.dsl.JvmTarget

plugins {
    id("java")
    id("org.jetbrains.kotlin.jvm") version "2.4.10"
    id("org.jetbrains.intellij.platform") version "2.18.1"
}

group = "dev.millet.tinyc"
version = "0.1.0"

repositories {
    mavenCentral()
    intellijPlatform {
        defaultRepositories()
    }
}

// Prefer the IDE installed on this machine: nothing to download, and `runIde`
// then launches the very IDE the plugin is meant for.
//
// It may be **said** in `gradle.properties`, and it is **looked for** when what
// is said is not there — the same bargain `TinyCTooling` strikes with nasm and
// the linker, and for the same reason. An installation moves, or is put
// somewhere the property's author did not expect, and a build that silently
// falls back to downloading a different IDE is a confusing way to learn that.
//
// A directory is an installation when it holds `product-info.json`, which every
// JetBrains IDE writes and nothing else does.
val localIde: String? = run {
    fun installation(directory: File?): File? =
        directory?.takeIf { File(it, "product-info.json").isFile }

    val said = providers.gradleProperty("tinycIdePath").orNull?.takeIf { it.isNotBlank() }

    // `none` is how to build the way a CI runner does — downloading the
    // platform — on a machine that has an IDE on it. Without it the fallback
    // below is unreachable here, and a path nobody can run is a path that rots.
    if (said == "none") return@run null

    installation(said?.let { file(it) })?.let { return@run it.absolutePath }

    val roots = listOfNotNull(
        System.getenv("LOCALAPPDATA")?.let { File(it, "Programs") },
        System.getenv("ProgramFiles")?.let { File(it, "JetBrains") },
        System.getenv("ProgramFiles(x86)")?.let { File(it, "JetBrains") },
        File(System.getProperty("user.home"), "Applications"),
        File("/opt"),
    )
    val found = roots.asSequence()
        .flatMap { (it.listFiles() ?: emptyArray()).asSequence() }
        .mapNotNull { installation(it) ?: installation(File(it, "Contents")) }
        // This plugin targets the IntelliJ platform, so IDEA is the closest
        // thing to it when a machine has several; any of them will do.
        .sortedBy { if (it.name.startsWith("IntelliJ IDEA")) 0 else 1 }
        .firstOrNull()

    if (said != null && found != null) {
        logger.lifecycle("tinycIdePath does not point at an IDE; using ${found.absolutePath}")
    }
    found?.absolutePath
}

// The TinyC checkout this plugin lives in, two directories up from `ide/intellij`.
val repositoryRoot: File = rootProject.projectDir.parentFile.parentFile

// -- the vocabulary, generated rather than repeated ---------------------------
//
// `TinyCTokens` used to be `src/token.rs` said again in Kotlin, which meant a
// keyword added to the language stopped being coloured until someone remembered
// to add it here too. It is now generated from `grammar/vocabulary.txt`, which
// the compiler writes out of the very table its lexer reads — so there is one
// list, it lives in the compiler, and this build has no opinion about it.
//
// The file is checked in, so building the plugin needs neither Rust nor cargo.
// A `cargo test` in the repository above is what notices it has gone stale.
// Nothing in here knows what TinyC's roles, words or symbols *are*. It makes a
// set and a word list per role it finds in the file, so a role added to the
// compiler arrives with a set of its own and no line written on this side.
val vocabularyFile: File = File(repositoryRoot, "grammar/vocabulary.txt")

val generateVocabulary = tasks.register("generateVocabulary") {
    description = "Generate TinyCTokens.kt from the compiler's exported vocabulary."
    val source = vocabularyFile
    val outputDirectory = layout.buildDirectory.dir("generated/vocabulary")
    inputs.file(source).withPropertyName("vocabulary")
    outputs.dir(outputDirectory).withPropertyName("generated")

    doLast {
        val records = source.readLines()
            .map { it.trim() }
            .filter { it.isNotEmpty() && !it.startsWith("#") }
            .map { it.split("\t") }

        val format = records.firstOrNull { it[0] == "format" }?.getOrNull(1)
        require(format == "1") {
            "${source.name} is format $format and this build reads 1; rebuild the plugin"
        }

        // `name, spelling, role` for a token; `name, noun` for one that carries
        // a value and so is spelled every way rather than one.
        val roles = records.filter { it[0] == "role" }.map { it[1] to it[2] }
        val tokens = records.filter { it[0] == "token" }.map { Triple(it[1], it[2], it[3]) }
        val valued = records.filter { it[0] == "valued" }.map { it[1] to it[2] }
        val builtins = records.filter { it[0] == "builtin" }.map { Triple(it[1], it[2], it[3]) }
        val specs = records.filter { it[0] == "spec" }.map { it[1] to it[2] }
        require(tokens.isNotEmpty() && valued.isNotEmpty()) { "${source.name} holds no tokens" }
        require(roles.isNotEmpty()) { "${source.name} names no roles" }

        val wordRoles = roles.filter { (_, shape) -> shape == "word" }.map { it.first }
        val unknown = tokens.map { it.third }.toSet() - roles.map { it.first }.toSet()
        require(unknown.isEmpty()) { "${source.name} uses roles it never declares: $unknown" }

        fun quote(text: String): String = '"' + text
            .replace("\\", "\\\\")
            .replace("\"", "\\\"")
            .replace("$", "\\$") + '"'

        fun wordsOf(role: String): List<Triple<String, String, String>> =
            tokens.filter { (_, _, itsRole) -> itsRole == role }

        fun namesOf(role: String): String = wordsOf(role).joinToString(", ") { it.first }

        /** `control` -> `CONTROL`, which is what a generated name is built from. */
        fun constantOf(role: String): String = role.uppercase()

        val out = StringBuilder()
        out.appendLine("// Generated from grammar/vocabulary.txt — do not edit.")
        out.appendLine("//")
        out.appendLine("// The compiler writes that file out of `vocabulary::SPELLED`, the same table")
        out.appendLine("// its lexer reads, so this is the language's own vocabulary rather than a")
        out.appendLine("// second copy of it. Adding a keyword to TinyC is adding it in Rust; it")
        out.appendLine("// arrives here by itself, and `cargo test` refuses a stale export.")
        out.appendLine("package dev.millet.tinyc.lang")
        out.appendLine()
        out.appendLine("import com.intellij.psi.tree.IElementType")
        out.appendLine("import com.intellij.psi.tree.TokenSet")
        out.appendLine()
        out.appendLine("object TinyCTokens {")

        out.appendLine("    // Words and punctuation, each spelled exactly one way.")
        for ((name, spelling, _) in tokens) {
            out.appendLine("    @JvmField val $name: IElementType = TinyCTokenType(${quote(spelling)})")
        }
        out.appendLine()
        out.appendLine("    // Tokens that carry a value, named by what a diagnostic calls them.")
        for ((name, noun) in valued) {
            out.appendLine("    @JvmField val $name: IElementType = TinyCTokenType(${quote(noun)})")
        }
        out.appendLine()
        out.appendLine("    /**")
        out.appendLine("     * A comment, which the compiler has no token for.")
        out.appendLine("     *")
        out.appendLine("     * Its lexer skips one as trivia, so a comment is the editor's concern")
        out.appendLine("     * and not the language's — which is why this is the one token type")
        out.appendLine("     * here that the vocabulary does not name.")
        out.appendLine("     */")
        out.appendLine("    @JvmField val LINE_COMMENT: IElementType = TinyCTokenType(\"comment\")")

        out.appendLine()
        out.appendLine("    /** Every word the lexer recognises, and the token it becomes. */")
        out.appendLine("    @JvmField val KEYWORDS: Map<String, IElementType> = linkedMapOf(")
        for ((name, spelling, role) in tokens.filter { it.third in wordRoles }) {
            out.appendLine("        ${quote(spelling)} to $name, // $role")
        }
        out.appendLine("    )")

        out.appendLine()
        out.appendLine("    /**")
        out.appendLine("     * Every symbol, the longest first.")
        out.appendLine("     *")
        out.appendLine("     * The order is what keeps `->` from lexing as `-` then `>`, so the")
        out.appendLine("     * lexer walks this list rather than deciding for itself.")
        out.appendLine("     */")
        out.appendLine("    @JvmField val PUNCTUATION: List<Pair<String, IElementType>> = listOf(")
        for ((name, spelling, _) in tokens.filterNot { it.third in wordRoles }
            .sortedByDescending { it.second.length }) {
            out.appendLine("        ${quote(spelling)} to $name,")
        }
        out.appendLine("    )")

        out.appendLine()
        out.appendLine("    // One set per role the compiler declares. A role added there arrives")
        out.appendLine("    // here with a set of its own; nothing in the build script knows what")
        out.appendLine("    // the roles are, only that a token has one.")
        for ((role, _) in roles) {
            out.appendLine(
                "    @JvmField val ${constantOf(role)}_TOKENS: TokenSet = " +
                    "TokenSet.create(${namesOf(role)})",
            )
        }
        out.appendLine()
        out.appendLine("    /** Every role, by the name the compiler gives it. */")
        out.appendLine("    @JvmField val ROLES: Map<String, TokenSet> = linkedMapOf(")
        for ((role, _) in roles) {
            out.appendLine("        ${quote(role)} to ${constantOf(role)}_TOKENS,")
        }
        out.appendLine("    )")
        out.appendLine()
        out.appendLine("    /** Every word, which is every name a program may not take. */")
        out.appendLine("    @JvmField val ALL_KEYWORDS: TokenSet = TokenSet.orSet(")
        out.appendLine("        ${wordRoles.joinToString(", ") { "${constantOf(it)}_TOKENS" }},")
        out.appendLine("    )")
        out.appendLine()
        out.appendLine("    @JvmField val COMMENTS: TokenSet = TokenSet.create(LINE_COMMENT)")
        out.appendLine()
        out.appendLine("    @JvmField val STRINGS: TokenSet = TokenSet.create(STRING_LITERAL, CHAR_LITERAL)")
        out.appendLine()
        out.appendLine("    @JvmField val LITERALS: TokenSet =")
        out.appendLine("        TokenSet.orSet(STRINGS, TokenSet.create(INT_LITERAL), LITERAL_TOKENS)")

        out.appendLine()
        out.appendLine("    /** The words of each role, in the order the compiler declares them. */")
        for (role in wordRoles) {
            val words = wordsOf(role).joinToString(", ") { quote(it.second) }
            out.appendLine("    @JvmField val ${constantOf(role)}_WORDS: List<String> = listOf($words)")
        }

        out.appendLine()
        out.appendLine("    /** A name already in the compiler's signature table. */")
        out.appendLine("    data class Builtin(")
        out.appendLine("        val name: String,")
        out.appendLine("        val parameters: List<String>,")
        out.appendLine("        val returns: String?,")
        out.appendLine("    )")
        out.appendLine()
        out.appendLine("    @JvmField val BUILTINS: List<Builtin> = listOf(")
        for ((name, parameters, returns) in builtins) {
            val taken = parameters.split(",").filter { it.isNotBlank() }
                .joinToString(", ") { quote(it) }
            val given = if (returns.isBlank()) "null" else quote(returns)
            out.appendLine("        Builtin(${quote(name)}, listOf($taken), $given),")
        }
        out.appendLine("    )")
        out.appendLine()
        out.appendLine("    @JvmField val BUILTIN_FUNCTIONS: List<String> = BUILTINS.map { it.name }")

        out.appendLine()
        out.appendLine("    /** What a `%` in a format string may be followed by, and what it writes. */")
        out.appendLine("    @JvmField val SPECIFIERS: Map<Char, String> = linkedMapOf(")
        for ((letter, writes) in specs) {
            out.appendLine("        '$letter' to ${quote(writes)},")
        }
        out.appendLine("    )")
        out.appendLine("}")

        val directory = outputDirectory.get().asFile.resolve("dev/millet/tinyc/lang")
        directory.mkdirs()
        directory.resolve("TinyCTokens.kt").writeText(out.toString())
    }
}

kotlin.sourceSets["main"].kotlin.srcDir(generateVocabulary)

dependencies {
    intellijPlatform {
        if (localIde != null) {
            local(localIde)
        } else {
            // `intellijIdeaCommunity` and not `intellijIdea` was right until
            // 2025.3, when JetBrains stopped publishing IDEA Community as a
            // distribution of its own. Asking for IC now fails to resolve at
            // all rather than falling back, so this is what a machine with no
            // IDE on it — a CI runner — has to ask for.
            intellijIdea(providers.gradleProperty("platformVersion"))
        }
        testFramework(TestFrameworkType.Platform)
    }

    testImplementation("junit:junit:4.13.2")
}

intellijPlatform {
    pluginConfiguration {
        id = "dev.millet.tinyc"
        name = "TinyC"
        version = project.version.toString()
        vendor {
            name = "Valentin Millet"
        }
        ideaVersion {
            // `since` is left to the platform being built against, which is the
            // only version this build can vouch for. Open ended the other way:
            // the plugin uses nothing a later platform is likely to take away,
            // and an `until` would retire it every twelve weeks.
            untilBuild = provider { null }
        }
    }

    pluginVerification {
        ides {
            recommended()
        }
    }
}

// The platform this builds against runs on Java 25, so the plugin is compiled
// for one — said outright rather than inherited from whichever JDK happens to
// run Gradle. Building against an older IDE means lowering this to match it.
kotlin {
    compilerOptions {
        jvmTarget = JvmTarget.JVM_25
    }
}

tasks.withType<org.jetbrains.kotlin.gradle.tasks.KotlinCompile>().configureEach {
    compilerOptions.jvmTarget = JvmTarget.JVM_25
}

java {
    sourceCompatibility = JavaVersion.VERSION_25
    targetCompatibility = JavaVersion.VERSION_25
}

tasks.withType<JavaCompile>().configureEach {
    options.release = 25
}

tasks.test {
    // The platform test fixtures reach into the JDK's internals.
    jvmArgs(
        "--add-opens=java.base/java.lang=ALL-UNNAMED",
        "--add-opens=java.base/java.util=ALL-UNNAMED",
        "--add-opens=java.desktop/java.awt=ALL-UNNAMED",
        "--add-opens=java.desktop/sun.awt=ALL-UNNAMED",
    )
    // `CompilerOutputTest` and `TinyCBuildTest` run the real `tinyc` on the real
    // examples, which are in the repository above this directory.
    systemProperty("tinyc.repo", repositoryRoot.absolutePath)
}
