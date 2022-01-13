object Dependencies {
    const val androidMaterial = "com.google.android.material:material:${Versions.Android.material}"
    const val commonsValidator = "commons-validator:commons-validator:${Versions.commonsValidator}"
    const val jodaTime = "joda-time:joda-time:${Versions.jodaTime}"
    const val junit = "junit:junit:${Versions.junit}"

    object AndroidX {
        const val appcompat = "androidx.appcompat:appcompat:${Versions.AndroidX.appcompat}"
        const val constraintlayout =
            "androidx.constraintlayout:constraintlayout:${Versions.AndroidX.constraintlayout}"
        const val coordinatorlayout =
            "androidx.coordinatorlayout:coordinatorlayout:${Versions.AndroidX.coordinatorlayout}"
        const val coreKtx = "androidx.core:core-ktx:${Versions.AndroidX.coreKtx}"
        const val fragmentKtx = "androidx.fragment:fragment-ktx:${Versions.AndroidX.fragment}"
        const val fragmentTestning =
            "androidx.fragment:fragment-testing:${Versions.AndroidX.fragment}"
        const val lifecycleRuntimeKtx =
            "androidx.lifecycle:lifecycle-runtime-ktx:${Versions.AndroidX.lifecycle}"
        const val lifecycleViewmodelKtx =
            "androidx.lifecycle:lifecycle-viewmodel-ktx:${Versions.AndroidX.lifecycle}"
        const val recyclerview =
            "androidx.recyclerview:recyclerview:${Versions.AndroidX.recyclerview}"
        const val junit = "androidx.test.ext:junit:${Versions.AndroidX.junit}"
        const val espressoCore =
            "androidx.test.espresso:espresso-core:${Versions.AndroidX.espresso}"
        const val espressoContrib =
            "androidx.test.espresso:espresso-contrib:${Versions.AndroidX.espresso}"
    }

    object Koin {
        const val core = "io.insert-koin:koin-core:${Versions.koin}"
        const val coreExt = "io.insert-koin:koin-core-ext:${Versions.koin}"
        const val androidXFragment = "io.insert-koin:koin-androidx-fragment:${Versions.koin}"
        const val androidXScope = "io.insert-koin:koin-androidx-scope:${Versions.koin}"
        const val androidXViewmodel = "io.insert-koin:koin-androidx-viewmodel:${Versions.koin}"
        const val test = "io.insert-koin:koin-test:${Versions.koin}"
    }

    object Kotlin {
        const val reflect = "org.jetbrains.kotlin:kotlin-reflect:${Versions.kotlin}"
        const val stdlib = "org.jetbrains.kotlin:kotlin-stdlib:${Versions.kotlin}"
        const val test = "org.jetbrains.kotlin:kotlin-test:${Versions.kotlin}"
    }

    object KotlinX {
        const val coroutinesAndroid =
            "org.jetbrains.kotlinx:kotlinx-coroutines-android:${Versions.kotlinx}"
        const val coroutinesTest =
            "org.jetbrains.kotlinx:kotlinx-coroutines-test:${Versions.kotlinx}"
    }

    object MockK {
        const val core = "io.mockk:mockk:${Versions.mockk}"
        const val android = "io.mockk:mockk-android:${Versions.mockk}"
    }

    object Plugin {
        const val android = "com.android.tools.build:gradle:${Versions.Plugin.android}"
        const val androidApplicationId = "com.android.application"
        const val playPublisher =
            "com.github.triplet.gradle:play-publisher:${Versions.Plugin.playPublisher}"
        const val playPublisherId = "com.github.triplet.play"
        const val kotlin = "org.jetbrains.kotlin:kotlin-gradle-plugin:${Versions.kotlin}"
        const val kotlinAndroidId = "kotlin-android"
        const val kotlinParcelizeId = "kotlin-parcelize"
        const val dependencyCheck =
            "org.owasp:dependency-check-gradle:${Versions.Plugin.dependencyCheck}"
        const val dependencyCheckId = "org.owasp.dependencycheck"
        const val gradleVersionsId = "com.github.ben-manes.versions"
    }
}
