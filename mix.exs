defmodule Quicksand.MixProject do
  use Mix.Project

  @version "0.1.1"
  @source_url "https://github.com/lpgauth/quicksand"

  def project do
    [
      app: :quicksand,
      version: @version,
      elixir: "~> 1.15",
      start_permanent: Mix.env() == :prod,
      deps: deps(),
      package: package(),
      description: "Sandboxed JavaScript execution for Elixir via QuickJS-NG",
      source_url: @source_url,
      homepage_url: @source_url
    ]
  end

  def application do
    [
      extra_applications: [:logger]
    ]
  end

  defp deps do
    [
      {:rustler_precompiled, "~> 0.8"},
      {:rustler, ">= 0.0.0", optional: true},
      {:ex_doc, "~> 0.35", only: :dev, runtime: false},
      {:dialyxir, "~> 1.4", only: :dev, runtime: false},
      {:stream_data, "~> 1.0", only: :test}
    ]
  end

  defp package do
    [
      licenses: ["MIT"],
      links: %{"GitHub" => @source_url},
      files: ~w(
        lib
        native/quicksand/src
        native/quicksand/Cargo.toml
        Cargo.toml
        Cargo.lock
        Cross.toml
        checksum-*.exs
        mix.exs
        README.md
        LICENSE
        .formatter.exs
      )
    ]
  end
end
