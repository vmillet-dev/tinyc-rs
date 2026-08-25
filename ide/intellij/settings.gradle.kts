// The plugin builds on its own; it is not part of the cargo workspace above it.
pluginManagement {
    repositories {
        gradlePluginPortal()
        mavenCentral()
    }
}

rootProject.name = "tinyc-intellij"
