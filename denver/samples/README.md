# denver samples: what was run inside a denver guest (2026-11-14)

Sources used in the operator verification (`denver/docs/archive/CLOJURE_AND_JAVAC_ON_DENVER.md`). The guest has no
`javac` launcher; use the module entry point. Applet links first: `/bin/busybox --install -s /bin`.

| Sample | Run |
|---|---|
| `java/Logo.java`, `java/HelloWorld.java`, `java/Workload.java` + `Worker.java` | `java -m jdk.compiler/com.sun.tools.javac.Main -d out X.java` then `java -cp out X` (Workload needs `-cp out:.` with `Worker`) |
| `clojure/hello.clj` | `java -cp clojure-1.12.0.jar:spec.alpha-0.5.238.jar:core.specs.alpha-0.4.74.jar clojure.main hello.clj` (the `pmap` keeps the JVM 60 s; add `(shutdown-agents)` to exit at once) |
| `clojure/server.clj` | same classpath, `clojure.main server.clj`; HTTP on guest port 8080 |
| `scala/HelloScala.scala` | `java -Xmx1500m -cp scala-compiler-2.13.14.jar:scala-library-2.13.14.jar:scala-reflect-2.13.14.jar scala.tools.nsc.Main -usejavacp -classpath scala-library-2.13.14.jar -d out HelloScala.scala`, then `java -cp out:scala-library-2.13.14.jar HelloScala` |
| `groovy/hello.groovy` | `java -cp groovy-4.0.22.jar groovy.ui.GroovyMain hello.groovy` |

Jars (Maven Central, SHA-1 checked against the `.sha1` files): `org/clojure/clojure/1.12.0`, `org/clojure/spec.alpha/0.5.238`,
`org/clojure/core.specs.alpha/0.4.74`, `org/scala-lang/scala-{compiler,library,reflect}/2.13.14`,
`org/apache/groovy/groovy/4.0.22`. Not committed (binary).
