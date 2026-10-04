pluginManagement {
    repositories {
        google()
        mavenCentral()
        gradlePluginPortal()
    }
}

dependencyResolutionManagement {
    repositoriesMode.set(RepositoriesMode.FAIL_ON_PROJECT_REPOS)
    repositories {
        google()
        mavenCentral()
        // Termux terminal-emulator and terminal-view (Apache-2.0) are published only on JitPack.
        maven("https://jitpack.io") {
            content {
                includeGroup("com.github.termux")
                includeGroup("com.github.termux.termux-app")
            }
        }
    }
}

rootProject.name = "sessiontap-android"
include(":app")
